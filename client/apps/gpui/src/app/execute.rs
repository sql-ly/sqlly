use super::*;
use std::ops::Range;

/// Whether [`AppView::execute_query`] is a fresh Run or the confirm path for an
/// armed mutation dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExecuteOrigin {
    UserRun,
    AiRun,
    AgentGrant { sql: String },
    MutationConfirm { sql: String },
    AutoRefresh { sql: String },
    NotebookCell { cell_id: String, sql: String },
}

impl ExecuteOrigin {
    /// Whether this run should bring a collapsed results pane back. Notebook
    /// cells render their own results inline, and auto-refresh ticks must
    /// not undo the user hiding the pane.
    pub(super) fn reveals_results(&self) -> bool {
        !matches!(self, Self::AutoRefresh { .. } | Self::NotebookCell { .. })
    }
}

/// What the Run action should do before executing SQL: proceed, or park the
/// batch behind the destructive-change confirmation dialog.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RunGateDecision {
    Proceed,
    ConfirmMutation {
        bottom_line: String,
        has_unbounded: bool,
        ai_review_required: bool,
        production: bool,
    },
    BlockProductionReadOnly {
        message: String,
    },
    BlockTenant {
        message: String,
    },
    BlockedPendingReview,
    BlockAutoRefresh,
}

/// Pure gate for [`AppView::execute_query`]: a batch needs confirmation only
/// when no review is already pending on this tab, a profile is active, and the
/// profile's effective safety features flag the analysis. Required tenant
/// scoping blocks when validation has already flagged a missing tenant
/// predicate — including on the mutation-confirm path.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_gate_decision(
    sql: &str,
    origin: &ExecuteOrigin,
    tab_id: &str,
    pending_review: Option<&PendingMutationReview>,
    profile: Option<&ConnectionProfile>,
    tenant_execute_block: Option<&str>,
    store: &ConnectionStore,
    unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
    now: sqlly_intel::query::safety::Instant,
) -> RunGateDecision {
    if let Some(message) = tenant_execute_block {
        return RunGateDecision::BlockTenant {
            message: message.to_string(),
        };
    }
    // Redis text is commands, not SQL: the same gates are judged by the
    // command allowlist instead of the SELECT-only parser.
    let command_engine = profile.is_some_and(|p| p.database_type.is_command_engine());
    // Read as the target engine reads it (every execution unit, every lexical
    // reading); with no profile, as every engine would.
    let db = profile.map(|p| p.database_type);
    let reads_only = |sql: &str| {
        if command_engine {
            !sqlly_intel::query::safety::redis::has_write_command(sql)
        } else if let Some(db) = db {
            sqlly_intel::query::safety::classify_select_only_for(db, sql).select_only
        } else {
            sqlly_intel::query::safety::classify_select_only(sql).select_only
        }
    };
    if matches!(origin, ExecuteOrigin::AutoRefresh { .. }) && !reads_only(sql) {
        return RunGateDecision::BlockAutoRefresh;
    }
    match origin {
        ExecuteOrigin::MutationConfirm { .. } => return RunGateDecision::Proceed,
        ExecuteOrigin::UserRun
        | ExecuteOrigin::AiRun
        | ExecuteOrigin::AgentGrant { .. }
        | ExecuteOrigin::NotebookCell { .. }
        | ExecuteOrigin::AutoRefresh { .. } => {
            if pending_review.is_some_and(|review| review.tab_id == tab_id) {
                return RunGateDecision::BlockedPendingReview;
            }
        }
    }
    let Some(profile) = profile else {
        return RunGateDecision::Proceed;
    };
    let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store);
    let production = safety.production_guard().decide(unlock, now);
    let writes = if command_engine {
        !reads_only(sql)
    } else {
        crate::mutation_review::contains_non_select_statement_for(profile.database_type, sql)
    };
    if production == sqlly_intel::query::safety::ProductionDecision::ReadOnly && writes {
        return RunGateDecision::BlockProductionReadOnly {
            message: crate::localization::text("shell.execute.production_read_only"),
        };
    }
    let analysis = if command_engine {
        crate::mutation_review::analyze_redis(sql)
    } else {
        crate::mutation_review::analyze_for(profile.database_type, sql)
    };
    match analysis {
        Some(analysis) if safety.requires_confirmation(&analysis, production) => {
            RunGateDecision::ConfirmMutation {
                bottom_line: analysis.bottom_line(),
                has_unbounded: analysis.has_dangerous_unbounded_change(),
                ai_review_required: safety.require_ai_review_for_data_changes,
                production: production
                    == sqlly_intel::query::safety::ProductionDecision::WritesUnlocked,
            }
        }
        _ => RunGateDecision::Proceed,
    }
}

/// Per-target gate fold for Run on…
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MultiRunGate {
    Proceed,
    Confirm {
        bottom_line: String,
        has_unbounded: bool,
        ai_review_required: bool,
        target_names: Vec<String>,
    },
    Refuse {
        message: String,
    },
}

#[allow(clippy::too_many_arguments)]
pub(super) fn multi_run_gate_decision(
    sql: &str,
    origin: &ExecuteOrigin,
    tab_id: &str,
    pending_review: Option<&PendingMutationReview>,
    targets: &[ConnectionProfile],
    tenant_execute_block: Option<&str>,
    store: &ConnectionStore,
    unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
    now: sqlly_intel::query::safety::Instant,
) -> MultiRunGate {
    if targets.is_empty() {
        return MultiRunGate::Refuse {
            message: crate::localization::text("shell.execute.pick_connection"),
        };
    }
    if matches!(origin, ExecuteOrigin::MutationConfirm { .. }) {
        return MultiRunGate::Proceed;
    }
    if pending_review.is_some_and(|review| review.tab_id == tab_id) {
        return MultiRunGate::Refuse {
            message: crate::localization::text("shell.execute.multi_target_review_pending"),
        };
    }
    let mut target_names = Vec::new();
    let mut bottom_line = String::new();
    let mut has_unbounded = false;
    let mut ai_review_required = false;
    for profile in targets {
        match run_gate_decision(
            sql,
            origin,
            tab_id,
            pending_review,
            Some(profile),
            tenant_execute_block,
            store,
            unlock,
            now,
        ) {
            RunGateDecision::BlockTenant { message } => {
                return MultiRunGate::Refuse {
                    message: crate::localization::format(
                        "shell.execute.multi_target_tenant_refusal_with_profile",
                        &[("message", &message), ("profile", &profile.name)],
                    ),
                };
            }
            RunGateDecision::BlockProductionReadOnly { message } => {
                return MultiRunGate::Refuse {
                    message: crate::localization::format(
                        "shell.execute.multi_target_production_refusal",
                        &[("message", &message), ("profile", &profile.name)],
                    ),
                };
            }
            RunGateDecision::BlockedPendingReview => {
                return MultiRunGate::Refuse {
                    message: crate::localization::text("shell.execute.multi_target_review_pending"),
                };
            }
            RunGateDecision::ConfirmMutation {
                bottom_line: bl,
                has_unbounded: hu,
                ai_review_required: air,
                ..
            } => {
                if target_names.is_empty() {
                    bottom_line = bl;
                }
                has_unbounded |= hu;
                ai_review_required |= air;
                target_names.push(profile.name.clone());
            }
            RunGateDecision::BlockAutoRefresh => {
                return MultiRunGate::Refuse {
                    message: crate::localization::text("shell.execute.auto_refresh_multi_target"),
                };
            }
            RunGateDecision::Proceed => {}
        }
    }
    if !target_names.is_empty() {
        MultiRunGate::Confirm {
            bottom_line,
            has_unbounded,
            ai_review_required,
            target_names,
        }
    } else {
        MultiRunGate::Proceed
    }
}

/// SQL to execute for this origin: the snapshotted batch on confirm, otherwise
/// the buffer-derived batch the caller already resolved.
pub(super) fn sql_for_execute_origin<'a>(
    origin: &'a ExecuteOrigin,
    buffer_sql: &'a str,
) -> &'a str {
    match origin {
        ExecuteOrigin::MutationConfirm { sql }
        | ExecuteOrigin::AutoRefresh { sql }
        | ExecuteOrigin::NotebookCell { sql, .. } => sql.as_str(),
        ExecuteOrigin::UserRun | ExecuteOrigin::AiRun | ExecuteOrigin::AgentGrant { .. } => {
            buffer_sql
        }
    }
}

/// Inputs for [`resolve_buffer_sql`]: how Run picks SQL from the editor before
/// origin substitution and parameter replacement.
pub(super) struct BufferSqlInput<'a> {
    /// The tab's engine runs one command per line (Redis): "current
    /// statement" is the caret's line, never a SQL statement span.
    pub command_lines: bool,
    pub origin: &'a ExecuteOrigin,
    pub proc_mode_active: bool,
    pub selection_range: Option<Range<usize>>,
    pub buffer_text: &'a str,
    pub cursor_offset: usize,
    pub run_current_statement: bool,
    pub forced_statement_offset: Option<usize>,
    pub selected_batch: Option<(&'a str, usize)>,
    pub dialect: sqlly_lang::sqlmodel::Dialect,
}

/// The span "run current statement" executes at `offset`: the SQL statement
/// for SQL engines, the whole line for command engines.
fn statement_span_for_input(input: &BufferSqlInput<'_>, offset: usize) -> (usize, usize) {
    if input.command_lines {
        crate::mutation_review::command_line_span_at(input.buffer_text, offset)
    } else {
        crate::mutation_review::statement_span_at_for(input.buffer_text, offset, input.dialect)
    }
}

/// SSMS convention: selection wins, else current statement or whole buffer.
/// Procedure mode always uses the full buffer; mutation confirm leaves buffer
/// empty so [`sql_for_execute_origin`] supplies the snapshotted batch.
/// Returns the SQL to run and its start byte offset in the editor buffer.
pub(super) fn resolve_buffer_sql(input: &BufferSqlInput<'_>) -> (String, usize) {
    if matches!(
        input.origin,
        ExecuteOrigin::MutationConfirm { .. }
            | ExecuteOrigin::AutoRefresh { .. }
            | ExecuteOrigin::NotebookCell { .. }
    ) {
        return (String::new(), 0);
    }
    if input.proc_mode_active {
        return (input.buffer_text.to_string(), 0);
    }
    if let Some((batch, start)) = input.selected_batch {
        return (batch.to_string(), start);
    }
    if let Some(offset) = input.forced_statement_offset {
        let (start, end) = statement_span_for_input(input, offset);
        return (input.buffer_text[start..end].to_string(), start);
    }
    if let Some(range) = input.selection_range.clone().filter(|range| {
        input
            .buffer_text
            .get(range.clone())
            .is_some_and(|text| !text.trim().is_empty())
    }) {
        return (input.buffer_text[range.clone()].to_string(), range.start);
    }
    if input.run_current_statement {
        let (start, end) = statement_span_for_input(input, input.cursor_offset);
        return (input.buffer_text[start..end].to_string(), start);
    }
    (input.buffer_text.to_string(), 0)
}

/// Byte span of the buffer that [`resolve_buffer_sql`] will run, for the
/// result-source highlight. `None` in procedure mode / confirm path (whole
/// buffer or snapshot — nothing to single out).
pub(super) fn executed_buffer_span(input: &BufferSqlInput<'_>) -> Option<(usize, usize)> {
    if matches!(
        input.origin,
        ExecuteOrigin::MutationConfirm { .. }
            | ExecuteOrigin::AutoRefresh { .. }
            | ExecuteOrigin::NotebookCell { .. }
    ) || input.proc_mode_active
    {
        return None;
    }
    if let Some((batch, start)) = input.selected_batch {
        let end = start + batch.len();
        return Some((start, end.min(input.buffer_text.len())));
    }
    if let Some(offset) = input.forced_statement_offset {
        return Some(statement_span_for_input(input, offset));
    }
    if let Some(range) = input.selection_range.clone().filter(|range| {
        input
            .buffer_text
            .get(range.clone())
            .is_some_and(|text| !text.trim().is_empty())
    }) {
        return Some((range.start, range.end));
    }
    if input.run_current_statement {
        return Some(statement_span_for_input(input, input.cursor_offset));
    }
    Some((0, input.buffer_text.len()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunLabelInput {
    pub running: bool,
    pub proc_mode: bool,
    pub has_selection: bool,
    pub run_current_statement: bool,
    pub statement_count: usize,
    pub file_backed: bool,
}

/// `(button label, tooltip)` for the query toolbar Run control.
pub(crate) fn run_label(input: RunLabelInput) -> (String, String) {
    let key = if input.running {
        "toolbar.run.cancel"
    } else if input.proc_mode {
        "toolbar.run.procedure"
    } else if input.has_selection {
        "toolbar.run.selection"
    } else if input.run_current_statement {
        "toolbar.run.statement"
    } else if input.file_backed && input.statement_count <= 1 {
        "toolbar.run.file"
    } else if input.statement_count > 1 {
        "toolbar.run.all"
    } else {
        "toolbar.run.default"
    };
    if input.statement_count > 1 && key == "toolbar.run.all" {
        return (
            crate::localization::format(
                "toolbar.run.all_label",
                &[("count", &input.statement_count.to_string())],
            ),
            crate::localization::format(
                "toolbar.run.all_tooltip",
                &[("count", &input.statement_count.to_string())],
            ),
        );
    }
    (
        crate::localization::text(&format!("{key}_label")),
        crate::localization::text(&format!("{key}_tooltip")),
    )
}

/// Apply `-- sqlly param:` substitution from the full buffer into the SQL that
/// will run. Skipped in procedure mode and on the mutation-confirm path.
pub(super) fn apply_query_param_substitution(
    origin: &ExecuteOrigin,
    proc_mode_active: bool,
    user_sql: &str,
    full_buffer: &str,
    db_type: sqlly_core::DatabaseType,
    param_values: &[(String, String)],
) -> Result<String, String> {
    if proc_mode_active
        || matches!(
            origin,
            ExecuteOrigin::MutationConfirm { .. }
                | ExecuteOrigin::AutoRefresh { .. }
                | ExecuteOrigin::NotebookCell { .. }
        )
    {
        return Ok(user_sql.to_string());
    }
    let vars = crate::query_params::parse_set_directives(full_buffer);
    let user_sql = if vars.is_empty() {
        user_sql.to_string()
    } else {
        crate::query_params::substitute_variables(db_type, user_sql, &vars)
    };
    let specs = crate::query_params::parse_param_directives(full_buffer);
    if specs.is_empty() {
        return Ok(user_sql.to_string());
    }
    crate::query_params::substitute_params(db_type, &user_sql, &specs, param_values)
}

/// Whether a confirmed `DatabaseChanged { name }` should retarget the tab away
/// from `current`. The engine already decided the session moved — SQL Server
/// ignoring case, MySQL comparing exactly, since `Sales` and `sales` are two
/// databases on a case-sensitive server — so only an identical name is a
/// no-op here. Folding case again would keep the picker on `sales` while the
/// session runs in `SALES`.
pub(super) fn database_change_retargets(current: Option<&str>, name: &str) -> bool {
    current != Some(name)
}

/// Clear the procedure-mode running flag; returns whether it was set.
pub(super) fn take_proc_mode_running(running: &mut bool) -> bool {
    let was = *running;
    if was {
        *running = false;
    }
    was
}

/// Apply the Include Statistics session-counter wrap after the run gate clears.
/// Returns the SQL to execute and, when wrapped, which dialect's filter to use.
pub(super) fn apply_include_statistics(
    sql: &str,
    kind: sqlly_lang::sqlmodel::ConnectionKind,
    include: bool,
    proc_mode_active: bool,
) -> (String, Option<crate::session_stats::StatsDialect>) {
    if !include || proc_mode_active {
        return (sql.to_string(), None);
    }
    match kind {
        sqlly_lang::sqlmodel::ConnectionKind::Tds => {
            if sqlly_intel::query::safety::should_skip_statistics_wrap(sql) {
                (sql.to_string(), None)
            } else {
                (
                    crate::session_stats::wrap_tds_session_stats(sql),
                    Some(crate::session_stats::StatsDialect::Tds),
                )
            }
        }
        sqlly_lang::sqlmodel::ConnectionKind::MySqlWire => (
            crate::session_stats::wrap_mysql_session_stats(sql),
            Some(crate::session_stats::StatsDialect::MySql),
        ),
        _ => (sql.to_string(), None),
    }
}

fn count_wrap_error_message(err: crate::count_query::CountWrapError) -> String {
    match err {
        crate::count_query::CountWrapError::NotSelectOnly(reason) => {
            if reason.is_empty() {
                crate::localization::text("shell.execute.not_select_only")
            } else {
                reason
            }
        }
        crate::count_query::CountWrapError::NotSingleStatement => {
            crate::localization::text("shell.execute.count_single_statement")
        }
        crate::count_query::CountWrapError::CteUnsupported => {
            crate::localization::text("shell.execute.count_cte_unsupported")
        }
    }
}

fn parse_count_from_event(event: &sqlly_services::QueryEvent) -> Option<u64> {
    let sqlly_services::QueryEvent::Rows { rows, .. } = event else {
        return None;
    };
    let cell = rows.first()?.first()?;
    match cell {
        sqlly_core::CellValue::I64(n) if *n >= 0 => Some(*n as u64),
        sqlly_core::CellValue::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl AppView {
    fn query_cancelled_status(&mut self, _cx: &mut Context<Self>) {
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.execute.query_cancelled"),
        );
    }

    pub(super) fn cancel_query_no_window(&mut self, cx: &mut Context<Self>) {
        if self.cancel_tab_query(self.active_tab, cx) {
            self.query_cancelled_status(cx);
            cx.notify();
        }
    }

    pub(super) fn cancel_tab_query(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let task_id = self
            .tabs
            .get_mut(index)
            .and_then(|tab| tab.active_query_id.take());
        let multi_run = self
            .tabs
            .get_mut(index)
            .and_then(|tab| tab.active_multi_run.take());
        let proc_was_running = self
            .tabs
            .get_mut(index)
            .is_some_and(|tab| take_proc_mode_running(&mut tab.proc_mode.running));
        let manual_cancel = self
            .tabs
            .get(index)
            .is_some_and(|tab| tab.transaction_mode == crate::tabs::TransactionMode::Manual);
        if task_id.is_none() && multi_run.is_none() && !proc_was_running {
            return false;
        }
        let services = if index == self.active_tab {
            self.active_profile
                .as_ref()
                .map(|p| self.services_for_profile(p))
                .unwrap_or_else(|| self.services.clone())
        } else if let Some(tab) = self.tabs.get(index) {
            if let Some(target) = &tab.connection_target {
                self.active_profile
                    .as_ref()
                    .filter(|p| p.id.0 == target.profile_id)
                    .map(|p| self.services_for_profile(p))
                    .unwrap_or_else(|| self.services.clone())
            } else {
                self.active_profile
                    .as_ref()
                    .map(|p| self.services_for_profile(p))
                    .unwrap_or_else(|| self.services.clone())
            }
        } else {
            self.services.clone()
        };
        if let Some(task_id) = task_id {
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let _ = services.queries.cancel(&task_id).await;
                    })
                    .await
                    .ok();
            })
            .detach();
        }
        if let Some(multi_run) = multi_run {
            let task_ids = multi_run.task_ids;
            let services = multi_run.services;
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let cancel_jobs: Vec<(usize, String)> = task_ids
                            .lock()
                            .unwrap()
                            .iter()
                            .enumerate()
                            .filter_map(|(i, id)| id.as_ref().map(|id| (i, id.clone())))
                            .collect();
                        for (i, id) in cancel_jobs {
                            if let Some(executor) = services.get(i) {
                                let _ = executor.queries.cancel(&id).await;
                            }
                        }
                    })
                    .await
                    .ok();
            })
            .detach();
        }
        if manual_cancel {
            self.note_manual_tx_cancel(index, cx);
        }
        true
    }

    /// Editor + results for this origin. Notebook cells (and mutation confirm
    /// that started from a notebook cell) use the cell pair; otherwise the
    /// tab's hidden/canonical pair. `MutationConfirm` reads `pending.mutation_review.notebook_cell`
    /// because confirm re-enters with `MutationConfirm { sql }` only.
    fn run_targets_for(
        &self,
        tab_idx: usize,
        origin: &ExecuteOrigin,
        cx: &App,
    ) -> Option<(Entity<SqlEditor>, Entity<ResultsView>)> {
        let tab = self.tabs.get(tab_idx)?;
        let cell_id = match origin {
            ExecuteOrigin::NotebookCell { cell_id, .. } => Some(cell_id.as_str()),
            ExecuteOrigin::MutationConfirm { .. } => self
                .pending
                .mutation_review
                .as_ref()
                .and_then(|r| r.notebook_cell.as_deref()),
            _ => None,
        };
        if let Some(id) = cell_id
            && let Some(nb) = &tab.notebook
            && let Some(pair) = nb.read(cx).cell_entities(id)
        {
            return Some(pair);
        }
        Some((tab.editor.clone(), tab.results.clone()))
    }

    pub(super) fn run_notebook_cell(
        &mut self,
        tab_idx: usize,
        cell_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(nb) = self.tabs.get(tab_idx).and_then(|t| t.notebook.clone()) else {
            return;
        };
        match nb.read(cx).sql_for_run(&cell_id) {
            Ok(sql) => {
                self.execute_query_with_origin(
                    tab_idx,
                    ExecuteOrigin::NotebookCell { cell_id, sql },
                    cx,
                );
            }
            Err(message) => {
                self.set_status(StatusLevel::Error, message);
                cx.notify();
            }
        }
    }

    /// `fallback_db_type` stands in for the active connection's engine when
    /// there is none — a multi-target run needs no active connection, only
    /// the engine to substitute query parameters for.
    fn current_user_sql(
        &mut self,
        tab_idx: usize,
        origin: &ExecuteOrigin,
        fallback_db_type: Option<sqlly_lang::sqlmodel::DatabaseType>,
        cx: &mut Context<Self>,
    ) -> Option<(String, usize)> {
        let database_type = self
            .active_profile
            .as_ref()
            .map(|profile| profile.database_type)
            .or(fallback_db_type)?;
        if let ExecuteOrigin::NotebookCell { sql, .. } = origin {
            if sql.trim().is_empty() {
                return None;
            }
            return Some((sql.clone(), 0));
        }
        let editor_entity = self.tabs.get(tab_idx)?.editor.clone();
        let proc_mode_active = self
            .tabs
            .get(tab_idx)
            .is_some_and(|tab| tab.proc_mode.enabled);
        let run_current_statement = self.toolbar.run_current_statement;
        let forced_statement_offset = self.pending.run_statement_offset.take();
        let selected_batch = self.pending.run_statement_selection.take();
        let (buffer_sql, region_start, executed_span) = {
            let editor = editor_entity.read(cx);
            let dialect = editor.database_type().family();
            let input = BufferSqlInput {
                command_lines: editor.database_type().is_command_engine(),
                origin,
                proc_mode_active,
                selection_range: editor.selection_range(),
                buffer_text: editor.text(),
                cursor_offset: editor.cursor(),
                run_current_statement,
                forced_statement_offset,
                selected_batch: selected_batch
                    .as_ref()
                    .map(|(sql, start)| (sql.as_str(), *start)),
                dialect,
            };
            let executed_span = executed_buffer_span(&input);
            let (buffer_sql, region_start) = resolve_buffer_sql(&input);
            (buffer_sql, region_start, executed_span)
        };
        if let Some(span) = executed_span
            && let Some(tab) = self.tabs.get_mut(tab_idx)
        {
            tab.last_executed_span = Some(span);
        }
        let user_sql = sql_for_execute_origin(origin, &buffer_sql).to_string();
        if user_sql.trim().is_empty() {
            return None;
        }
        let full_buffer = editor_entity.read(cx).text().to_string();
        let param_values = self
            .tabs
            .get(tab_idx)
            .map(|tab| tab.query_params.values())
            .unwrap_or_default();
        match apply_query_param_substitution(
            origin,
            proc_mode_active,
            &user_sql,
            &full_buffer,
            database_type,
            &param_values,
        ) {
            Ok(substituted) => {
                let mut history_changed = false;
                for (name, value) in &param_values {
                    if !value.trim().is_empty() && self.param_history.remember(name, value) {
                        history_changed = true;
                    }
                }
                if history_changed {
                    self.param_history.save();
                }
                Some((substituted, region_start))
            }
            Err(message) => {
                let specs = crate::query_params::parse_param_directives(&full_buffer);
                if let Some(name) = crate::query_params::first_unresolved(&specs, &param_values)
                    && let Some(tab) = self.tabs.get_mut(tab_idx)
                {
                    tab.query_params.pending_focus = Some(name);
                }
                self.set_status(StatusLevel::Error, message);
                cx.notify();
                None
            }
        }
    }
    pub(super) fn execute_query(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        self.execute_query_with_origin(tab_idx, ExecuteOrigin::UserRun, cx);
    }

    pub(super) fn execute_query_keep_results(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(tab_idx) {
            tab.results
                .update(cx, |results, _cx| results.keep_results_on_next_run());
        }
        self.execute_query(tab_idx, cx);
    }

    pub(super) fn execute_query_with_origin(
        &mut self,
        tab_idx: usize,
        origin: ExecuteOrigin,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.get(tab_idx).is_some_and(|tab| tab.snapshot_only) {
            self.pending.run_statement_offset = None;
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.execute.snapshot_read_only"),
            );
            cx.notify();
            return;
        }
        // Marked natural-language dates ("yesterday") re-resolve against
        // today before the text is read, when that preference is on.
        if let Some(tab) = self.tabs.get(tab_idx) {
            let editor = tab.editor.clone();
            editor.update(cx, |ed, cx| ed.refresh_dates_before_run(cx));
        }
        // Check for a pending resume connection confirmation. Mirrors Swift
        // `executeQuery()` (line 3029): only prompt when the app is
        // *disconnected* (no active profile). If already connected, skip the
        // prompt and proceed with the current connection.
        if self.pending.resume_confirmation.is_none()
            && let Some(tab) = self.tabs.get(tab_idx)
            && tab.has_pending_resume_confirmation
            && let Some(ref pending_conn) = tab.pending_resume_connection
        {
            if self.active_profile.is_none() {
                // Disconnected — prompt or auto-switch.
                if tab.suppress_resume_connection_confirmation {
                    self.activate_remembered_connection(tab_idx, cx);
                } else {
                    self.pending.resume_confirmation = Some(PendingResumeConnectionConfirmation {
                        tab_idx,
                        server: pending_conn.server.clone(),
                        database: pending_conn.database.clone(),
                    });
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format(
                            "shell.execute.resume_switch_prompt",
                            &[
                                ("server", &pending_conn.server),
                                (
                                    "database",
                                    &pending_conn.database.clone().unwrap_or_else(|| {
                                        crate::localization::text(
                                            "shell.execute.resume_switch_default_database",
                                        )
                                    }),
                                ),
                            ],
                        ),
                    );
                    cx.notify();
                    return;
                }
            } else {
                // Already connected — clear the pending flag.
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.has_pending_resume_confirmation = false;
                }
            }
        }

        let profile = match self.active_profile.clone() {
            Some(p) => p,
            None => {
                self.pending.run_statement_offset = None;
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.execute.no_active_connection"),
                );
                cx.notify();
                return;
            }
        };

        let Some((editor_entity, results_entity)) = self.run_targets_for(tab_idx, &origin, cx)
        else {
            self.pending.run_statement_offset = None;
            return;
        };

        // Auto format (query-toolbar "Fmt" toggle): format the buffer right
        // before running, through the same command Format Active Query uses —
        // one undo step, so ⌘Z restores the pre-format text. Skipped while a
        // selection is active: rewriting the buffer would invalidate the
        // selection offsets that pick what runs. Also skipped on the mutation-
        // confirm path — the snapshotted SQL must not be replaced.
        if self.toolbar.auto_format
            && matches!(origin, ExecuteOrigin::UserRun)
            && self.pending.mutation_review.is_none()
            && self.pending.run_statement_offset.is_none()
            && self.pending.run_statement_selection.is_none()
            && !editor_entity.read(cx).has_selection()
            && !editor_entity.read(cx).database_type().is_command_engine()
        {
            editor_entity.update(cx, |ed, cx| ed.format_sql_buffer(cx));
        }

        let proc_mode_active = self
            .tabs
            .get(tab_idx)
            .is_some_and(|tab| tab.proc_mode.enabled);
        let Some((user_sql, region_start)) = self.current_user_sql(tab_idx, &origin, None, cx)
        else {
            return;
        };
        let run_region = crate::execution_error::RunRegion {
            start: region_start,
            revision: editor_entity.read(cx).content_revision(),
        };
        let run_sql_for_locate = user_sql.clone();
        // Procedure mode with a parseable header switches Run to the deploy +
        // invoke sequence. A header that no longer parses (or a buffer that
        // stopped being a procedure script) falls back to running the script
        // verbatim — the script may be intentionally partial.
        let proc_header = if proc_mode_active {
            let proc_dialect = self.active_proc_dialect();
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                tab.proc_mode
                    .refresh_from_script_for(&user_sql, proc_dialect);
            }
            self.tabs
                .get(tab_idx)
                .and_then(|tab| tab.proc_mode.parsed.clone())
                .and_then(|parsed| parsed.ok())
        } else {
            None
        };

        let tenant_execute_block = self.active_profile.as_ref().and_then(|profile| {
            crate::tenant_scoping::execute_block_for_fail_closed(
                profile,
                editor_entity.read(cx).validation_diagnostics(),
            )
        });

        let store = ConnectionStore::global();
        let tab_id = self.tabs[tab_idx].id.clone();
        match run_gate_decision(
            &user_sql,
            &origin,
            &tab_id,
            self.pending.mutation_review.as_ref(),
            self.active_profile.as_ref(),
            tenant_execute_block.as_deref(),
            store,
            self.production_unlock.as_ref(),
            sqlly_intel::query::safety::Instant::now(),
        ) {
            RunGateDecision::ConfirmMutation {
                bottom_line,
                has_unbounded,
                ai_review_required,
                production,
            } => {
                self.mutation_review_generation += 1;
                let generation = self.mutation_review_generation;
                self.pending.mutation_review = Some(PendingMutationReview {
                    sql: user_sql.clone(),
                    tab_id: self.tabs[tab_idx].id.clone(),
                    bottom_line,
                    has_unbounded,
                    ai_review_required,
                    production,
                    ai_explanation: None,
                    ai_generation: generation,
                    notebook_cell: match &origin {
                        ExecuteOrigin::NotebookCell { cell_id, .. } => Some(cell_id.clone()),
                        _ => None,
                    },
                    multi_targets: None,
                });
                if ai_review_required {
                    self.set_status(
                        StatusLevel::Busy,
                        crate::localization::text("shell.execute.generating_ai_review"),
                    );
                    self.spawn_ai_mutation_review(user_sql.clone(), generation, cx);
                } else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.execute.confirm_mutation"),
                    );
                }
                cx.notify();
                return;
            }
            RunGateDecision::BlockTenant { message } => {
                self.set_status(StatusLevel::Error, message);
                cx.notify();
                return;
            }
            RunGateDecision::BlockProductionReadOnly { message } => {
                self.set_status(StatusLevel::Error, message);
                cx.notify();
                return;
            }
            RunGateDecision::BlockedPendingReview => {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.execute.review_pending"),
                );
                cx.notify();
                return;
            }
            RunGateDecision::BlockAutoRefresh => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.auto_refresh_secs = None;
                }
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.execute.auto_refresh_disabled_not_select"),
                );
                cx.notify();
                return;
            }
            RunGateDecision::Proceed => {}
        }

        if let Some(grant) = self.agent_write_grant.as_mut() {
            if let Err(_error) = grant.consume(&user_sql, &profile) {
                self.agent_write_grant = None;
                let tab_id = self.tabs[tab_idx].id.clone();
                #[cfg(not(target_arch = "wasm32"))]
                self.reject_mcp_pending_write_if_tab(&tab_id, "could not execute MCP proposal");
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.execute.agent_grant_mismatch"),
                );
                cx.notify();
                return;
            }
            self.agent_write_grant = None;
        }

        // "Include statistics": applied only after the run gate so SELECT-only
        // and mutation confirmation see the user's SQL. SQL Server and
        // MySQL/MariaDB wrap the run in session-counter snapshots whose
        // sentinel result sets the event filter peels back out.
        let (sql, stats_dialect) = apply_include_statistics(
            &user_sql,
            profile.database_type.connection_kind(),
            self.toolbar.include_statistics,
            proc_mode_active,
        );

        if let Some(ref profile) = self.active_profile {
            self.log.log_verbose(
                "query",
                &crate::localization::format(
                    "shell.execute.log_executing",
                    &[
                        ("count", &sql.len().to_string()),
                        ("server", &profile.server),
                        (
                            "database",
                            &profile.database.clone().unwrap_or_else(|| {
                                crate::localization::text("shell.execute.log_default_database")
                            }),
                        ),
                    ],
                ),
            );
        }

        // All-time tally shown by the free-tier pay dialog. Sits with the
        // history log: per-run bookkeeping written once the run has cleared
        // every gate above (connection, empty buffer, mutation review).
        crate::license::increment_query_count();
        if origin.reveals_results() {
            self.show_results();
        }

        // Ensure the Azure SQL token is fresh (re-minting if the previous one
        // has expired) before running the query. On a live session with a
        // valid cached token this runs synchronously; for non-Entra
        // connections it is an immediate pass-through.
        // Consume the one-shot rollback-wrap bypass here, at the last gate
        // before dispatch, so a mutation-review round-trip (which re-enters
        // execute_query) does not drop it. The active profile is left
        // untouched — only the profile handed to this run drops the wrap.
        let bypass_rollback = std::mem::take(&mut self.execute_rollback_bypass_once);
        let history_provenance = match origin {
            ExecuteOrigin::AiRun => crate::query_history::HistoryProvenance::Ai,
            _ => crate::query_history::HistoryProvenance::Manual,
        };
        let tab_id = self.tabs[tab_idx].id.clone();
        self.with_entra_sql_token(profile, cx, move |app, mut profile, cx| {
            // The token mint may have gone async (Entra browser/device-code,
            // cloud IAM): re-resolve the tab by its stable id so a close or
            // reorder during the round-trip cannot retarget another tab.
            let Some(tab_idx) = app.tab_index_by_id(&tab_id) else {
                app.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.execute.tab_closed_before_token"),
                );
                cx.notify();
                return;
            };
            // Keep the active profile's token fresh for later reads
            // (completion refresh, database switch) — but only if the user
            // hasn't switched connections during an async re-mint.
            if app.active_profile.as_ref().map(|p| p.id) == Some(profile.id) {
                app.active_profile = Some(profile.clone());
            }
            if bypass_rollback && profile.auto_wrap_rollback {
                profile.auto_wrap_rollback = false;
                app.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.execute.rollback_bypassed"),
                );
            }
            let store = ConnectionStore::global();
            let now = sqlly_intel::query::safety::Instant::now();
            let read_only_unlock = app
                .active_profile
                .as_ref()
                .and_then(|p| app.read_only_unlocks.get(&p.id.0.to_string()));
            crate::mutation_review::apply_read_only_unlock(&mut profile, read_only_unlock, now);
            crate::mutation_review::apply_inherited_execution_policy(
                &mut profile,
                store,
                app.production_unlock.as_ref(),
                now,
            );
            let is_manual = app
                .tabs
                .get(tab_idx)
                .is_some_and(|t| t.transaction_mode == crate::tabs::TransactionMode::Manual);
            if is_manual {
                if let Some(reason) = app.manual_mode_availability(tab_idx).reason() {
                    app.set_status(StatusLevel::Error, reason);
                    cx.notify();
                    return;
                }
                profile.auto_wrap_rollback = false;
                if let Some(header) = proc_header {
                    app.run_with_manual_transaction_if_needed(
                        tab_idx,
                        profile,
                        cx,
                        move |app, profile, cx| {
                            app.execute_proc_query(
                                tab_idx,
                                sql,
                                header,
                                profile,
                                results_entity,
                                cx,
                            );
                        },
                    );
                } else {
                    app.run_with_manual_transaction_if_needed(
                        tab_idx,
                        profile,
                        cx,
                        move |app, profile, cx| {
                            app.spawn_query_execution(
                                tab_idx,
                                sql,
                                profile,
                                results_entity,
                                stats_dialect,
                                history_provenance,
                                run_sql_for_locate,
                                run_region,
                                cx,
                            );
                        },
                    );
                }
                return;
            }
            if let Some(header) = proc_header {
                app.execute_proc_query(tab_idx, sql, header, profile, results_entity, cx);
            } else {
                app.spawn_query_execution(
                    tab_idx,
                    sql,
                    profile,
                    results_entity,
                    stats_dialect,
                    history_provenance,
                    run_sql_for_locate,
                    run_region,
                    cx,
                );
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn underline_execution_error(
        &mut self,
        tab_idx: usize,
        run_sql: &str,
        region: crate::execution_error::RunRegion,
        db_type: sqlly_core::DatabaseType,
        auto_wrap_rollback: bool,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(location) = crate::execution_error::locate(&crate::execution_error::LocateInput {
            db_type,
            auto_wrap_rollback,
            run_sql,
            message,
        }) else {
            return false;
        };
        let Some(tab) = self.tabs.get(tab_idx) else {
            return false;
        };
        let editor = tab.editor.clone();
        let results = tab.results.clone();
        if editor.read(cx).content_revision() != region.revision {
            return false;
        }
        let text = editor.read(cx).text().to_string();
        let start = region.start.saturating_add(location.start);
        let end = region.start.saturating_add(location.end);
        if start > text.len() {
            return false;
        }
        let clamped_start = start.min(text.len());
        let clamped_end = end.min(text.len()).max(clamped_start);
        let line = text[..clamped_start].matches('\n').count();
        let diagnostic = crate::editor::ValidationDiagnostic {
            severity: crate::editor::DiagnosticSeverity::Error,
            source: crate::editor::DiagnosticSource::Execution,
            message: message.lines().next().unwrap_or(message).to_string(),
            line,
            identifier: None,
            start_byte: clamped_start,
            end_byte: clamped_end,
            explainer: None,
            fix: None,
        };
        editor.update(cx, |e, cx| e.set_execution_error(diagnostic, cx));
        results.update(cx, |r, cx| {
            r.set_execution_error_offset(Some(clamped_start), cx)
        });
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_query_execution(
        &mut self,
        tab_idx: usize,
        sql: String,
        profile: ConnectionProfile,
        results_entity: Entity<ResultsView>,
        stats_dialect: Option<crate::session_stats::StatsDialect>,
        history_provenance: crate::query_history::HistoryProvenance,
        run_sql: String,
        run_region: crate::execution_error::RunRegion,
        cx: &mut Context<Self>,
    ) {
        // Seal a Run version at dispatch. The version id is parked on the tab
        // so `attach_run_results` can hang the results snapshot on it when the
        // query completes.
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.query_history_prefs.enabled {
                let now = chrono::Utc::now().timestamp_millis();
                let sealed = self.history_store_for_tab(tab_idx).and_then(|store| {
                    match store.seal(crate::query_version_history::VersionKind::Run, &sql, now) {
                        Ok(crate::query_version_history::SealOutcome::Sealed(id)) => Some(id),
                        _ => None,
                    }
                });
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.pending_run_version_id = sealed;
                }
                self.refresh_history_panel(tab_idx, cx);
            }
        }
        // The connection this run executes against — used to guard a late
        // `DatabaseChanged` from retargeting the db picker after the user has
        // already switched to a different connection.
        let run_profile_id = profile.id;
        let tab_id = self
            .tabs
            .get(tab_idx)
            .map(|tab| tab.id.clone())
            .unwrap_or_default();
        // The run stamp ties every event this pump forwards to THIS run: if
        // the user re-runs while this query is still draining, the stale
        // pump's events (Rows, SpillWindow, SpillManifest, ...) are dropped
        // by `handle_event_for_run` instead of landing in the new run's view.
        let services = self.services_for_profile(&profile);
        let row_edit_block = self.row_edit_connection_block();
        let formula_variables = self
            .tabs
            .get(tab_idx)
            .map(|tab| {
                crate::computed_columns::formula_variables(
                    tab.editor.read(cx).text(),
                    &tab.query_params.values(),
                )
            })
            .unwrap_or_default();
        let run = results_entity.update(cx, |r, cx| {
            r.set_formula_variables(formula_variables);
            r.set_query_sql(&sql, cx);
            r.set_row_edit_connection_block(row_edit_block);
            r.set_source_db_type(profile.database_type);
            r.set_layout_scope(Some(crate::column_layout::LayoutScope::from_profile(
                &profile,
            )));
            r.set_fk_label_context(Some(profile.clone()), Some(services));
            r.set_masking_overrides(profile.masking.clone());
            r.start_query(cx);
            r.run_generation()
        });
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.last_run_sql = Some(sql.clone());
        }
        self.set_status(
            StatusLevel::Busy,
            crate::localization::text("shell.execute.query_running"),
        );
        self.pending.mutation_review = None;

        // A superseded run's producer would otherwise keep streaming from the
        // server (and spilling to disk) until it completes on its own —
        // cancel it now that its events can no longer land anywhere.
        if let Some(prior) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|t| t.active_query_id.take())
        {
            // Route the cancel to the same services that started the prior run:
            // a Via-Relay run's cancel must reach the relay-backed executor, not
            // the in-process one. (For Direct this resolves to `self.services`.)
            let services = self.services_for_profile(&profile);
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let _ = services.queries.cancel(&prior).await;
                    })
                    .await
                    .ok();
            })
            .detach();
        }
        if let Some(count_id) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|t| t.total_count_query_id.take())
        {
            let services = self.services_for_profile(&profile);
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let _ = services.queries.cancel(&count_id).await;
                    })
                    .await
                    .ok();
            })
            .detach();
        }

        // Execution routing: Via-Relay profiles run through the remote-backed
        // services (when a relay session is established); Direct profiles use the
        // in-process services exactly as before.
        let services = self.services_for_profile(&profile);
        let history_sql = sql.clone();
        let history_server = profile.server.clone();
        let history_database = profile.database.clone();
        // Sealed at dispatch for the completion summary: the pump only sees
        // events, not the run plan, and re-splitting at completion could
        // disagree with what actually ran if the buffer changed meanwhile.
        let statement_count =
            crate::editor::statements::statement_spans_for(&sql, profile.database_type.family())
                .len()
                .max(1);
        // DDL anywhere in the batch (CREATE TABLE, ALTER TABLE ... ADD/DROP
        // COLUMN, DROP VIEW, ...) leaves the explorer and IntelliSense stale.
        let changes_schema =
            sqlly_intel::query::safety::changes_schema_for(profile.database_type, &sql);
        let locate_sql = std::sync::Arc::<str>::from(run_sql);
        let locate_region = run_region;
        let locate_db_type = profile.database_type;
        let locate_auto_wrap = profile.auto_wrap_rollback;
        let row_buffer = 256usize;
        // Per-tab warm-connection lane: each tab reuses its own warm client
        // across runs; tabs never share sessions (temp tables, SET options,
        // SESSION_CONTEXT stay tab-local), matching the engine's
        // `connection_key` isolation.
        let tab_lane = self.tabs.get(tab_idx).map(|tab| tab.id.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<sqlly_services::QueryEvent>(64);
        // `execute` mints the cancellation task id; route it back to the tab
        // so `query.cancel` has something to act on.
        let (id_tx, id_rx) = tokio::sync::oneshot::channel::<String>();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    match services
                        .queries
                        .execute_in_session(&profile, tab_lane.as_deref(), &sql, row_buffer)
                        .await
                    {
                        Ok(handle) => {
                            let _ = id_tx.send(handle.task_id.clone());
                            let mut events = handle.events;
                            // Include Statistics: peel the sentinel snapshot
                            // sets and emit the counter-delta summary as a
                            // message.
                            let mut stats_filter =
                                stats_dialect.map(crate::session_stats::SessionStatsFilter::new);
                            'pump: while let Some(event) = events.recv().await {
                                let outputs = match &mut stats_filter {
                                    Some(filter) => filter.process(event),
                                    None => vec![event],
                                };
                                for out in outputs {
                                    if tx.send(out).await.is_err() {
                                        break 'pump;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(sqlly_services::QueryEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                        }
                    }
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        let log = self.log.clone();
        let tab_id_for_pump = tab_id.clone();
        cx.spawn(async move |_this, _cx| {
            // Store the task id for cancellation — but only while this run is
            // still current, so a slow superseded pump can't clobber the id
            // of the run that replaced it.
            if let Ok(task_id) = id_rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        let Some(tab_idx) = this.tab_index_by_id(&tab_id_for_pump) else {
                            return;
                        };
                        if results_entity.read(cx).run_generation() == run
                            && let Some(tab) = this.tabs.get_mut(tab_idx)
                        {
                            tab.active_query_id = Some(task_id);
                        }
                    })
                    .ok();
            }
            let mut completed_ms: Option<u64> = None;
            let mut last_error: Option<String> = None;
            let mut saw_cancel = false;
            let mut affected_rows: Option<u64> = None;
            let mut located = false;
            while let Some(event) = rx.recv().await {
                // Kept for the firewall check below: `event` is moved into the
                // results view before the update closure can look at it.
                let mut error_message: Option<String> = None;
                // A `USE` that retargeted the session — reflected into the db
                // picker / status bar / IntelliSense after the results update.
                let mut db_changed: Option<String> = None;
                let mut underline_text: Option<String> = None;
                match &event {
                    sqlly_services::QueryEvent::Error { message } => {
                        log.log_error("query", message);
                        error_message = Some(message.clone());
                        last_error = Some(message.clone());
                        underline_text = Some(message.clone());
                    }
                    sqlly_services::QueryEvent::Message { text } => {
                        underline_text = Some(text.clone());
                    }
                    sqlly_services::QueryEvent::Completed { elapsed_ms } => {
                        log.log_verbose(
                            "query",
                            &crate::localization::format(
                                "shell.execute.log_completed",
                                &[("milliseconds", &elapsed_ms.to_string())],
                            ),
                        );
                        completed_ms = Some(u64::try_from(*elapsed_ms).unwrap_or(u64::MAX));
                    }
                    sqlly_services::QueryEvent::Cancelled => {
                        log.log_verbose(
                            "query",
                            &crate::localization::text("shell.execute.log_cancelled"),
                        );
                        saw_cancel = true;
                    }
                    sqlly_services::QueryEvent::DatabaseChanged { name } => {
                        log.log_verbose(
                            "query",
                            &crate::localization::format(
                                "shell.execute.log_database_changed",
                                &[("database", name)],
                            ),
                        );
                        db_changed = Some(name.clone());
                    }
                    sqlly_services::QueryEvent::RowCount { rows_affected, .. } => {
                        affected_rows =
                            Some(affected_rows.unwrap_or(0).saturating_add(*rows_affected));
                    }
                    _ => {}
                }
                let mut cx = async_cx.clone();
                if weak_self
                    .update(&mut cx, |this, cx| {
                        results_entity.update(cx, |r, cx| r.handle_event_for_run(run, event, cx));
                        this.log_console.update(cx, |view, cx| view.refresh(cx));
                        if !located
                            && let Some(text) = underline_text
                            && let Some(tab_idx) = this.tab_index_by_id(&tab_id_for_pump)
                            && this.underline_execution_error(
                                tab_idx,
                                &locate_sql,
                                locate_region,
                                locate_db_type,
                                locate_auto_wrap,
                                &text,
                                cx,
                            )
                        {
                            located = true;
                        }
                        if let Some(message) = error_message {
                            this.refresh_explorer_after_run = false;
                            this.arm_firewall_prompt(&message, None, cx);
                        }
                        // Retarget the session db — but only while this run
                        // is still current, still on the connection it started
                        // on, the completing tab is still focused, and the
                        // database actually differs.
                        if let Some(name) = db_changed
                            && results_entity.read(cx).run_generation() == run
                            && this.active_profile.as_ref().map(|p| p.id) == Some(run_profile_id)
                            && this
                                .tabs
                                .get(this.active_tab)
                                .is_some_and(|t| t.id == tab_id_for_pump)
                            && database_change_retargets(
                                this.active_profile
                                    .as_ref()
                                    .and_then(|p| p.database.as_deref()),
                                &name,
                            )
                        {
                            this.switch_database(name, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    let history_outcome = if last_error.is_some() {
                        crate::query_history::HistoryOutcome::Failed
                    } else if saw_cancel {
                        crate::query_history::HistoryOutcome::Cancelled
                    } else {
                        crate::query_history::HistoryOutcome::Succeeded
                    };
                    this.query_history
                        .log(&crate::query_history::HistoryRecord {
                            sql: &history_sql,
                            server: &history_server,
                            database: history_database.as_deref(),
                            duration_ms: completed_ms,
                            outcome: history_outcome,
                            provenance: history_provenance,
                            affected_rows,
                            rollback_sql: None,
                        });
                    // Even a failed or cancelled batch may have committed
                    // its DDL before stopping, so refresh regardless.
                    if changes_schema {
                        this.refresh_schema_after_change(run_profile_id.0, cx);
                    }
                    // A superseded pump ending late must not clear the id (or
                    // rewrite the status) of the run that replaced it.
                    if results_entity.read(cx).run_generation() == run {
                        if let Some(tab_idx) = this.tab_index_by_id(&tab_id_for_pump)
                            && let Some(tab) = this.tabs.get_mut(tab_idx)
                        {
                            tab.active_query_id = None;
                            this.touch_manual_tx(tab_idx);
                        }
                        if let Some(ref message) = last_error {
                            this.refresh_explorer_after_run = false;
                            // Remember the failure together with the SQL that
                            // produced it, so "Explain with AI" seeds the drawer
                            // with the pair that actually failed — not whatever
                            // the (possibly edited or different) active tab
                            // holds when the command runs later.
                            this.last_query_error = Some(crate::app::ai::LastQueryError {
                                error: message.clone(),
                                sql: history_sql.clone(),
                                tab_id: tab_id_for_pump.clone(),
                            });
                            // The flat "Query complete" used to overwrite the
                            // Busy status even after a failed run, masking the
                            // failure everywhere but the results pane.
                            this.notifications.push(
                                crate::notification_center::NotificationKind::QueryFailed,
                                message.clone(),
                                chrono::Utc::now(),
                            );
                            if !this.window_is_active && this.os_notify.enabled {
                                crate::notification_center::notify_system(
                                    &crate::localization::text("shell.execute.query_failed_title"),
                                    message,
                                    cx,
                                );
                            }
                            this.set_status(StatusLevel::Error, message);
                        } else if saw_cancel {
                            this.refresh_explorer_after_run = false;
                            #[cfg(not(target_arch = "wasm32"))]
                            this.reject_mcp_pending_write_if_tab(&tab_id_for_pump, "user declined");
                            this.query_cancelled_status(cx);
                        } else {
                            let rows = results_entity.read(cx).total_rows();
                            let message = match completed_ms {
                                Some(ms) => run_summary(statement_count, affected_rows, rows, ms),
                                None => crate::localization::text("shell.execute.query_complete"),
                            };
                            // A successful run supersedes any recorded failure:
                            // "Explain Last Error" must never pair an old error
                            // with SQL that now runs.
                            this.last_query_error = None;
                            if this.refresh_explorer_after_run {
                                this.refresh_explorer_after_run = false;
                                this.sidebar
                                    .update(cx, |sidebar, cx| sidebar.refresh_connections(cx));
                            }
                            // A run long enough that the user plausibly
                            // looked away also lands in the bell.
                            if crate::notification_center::completion_is_noteworthy(completed_ms) {
                                this.notifications.push(
                                    crate::notification_center::NotificationKind::QueryCompleted,
                                    message.clone(),
                                    chrono::Utc::now(),
                                );
                                if !this.window_is_active && this.os_notify.enabled {
                                    crate::notification_center::notify_system(
                                        "SQLLY", &message, cx,
                                    );
                                }
                            }
                            this.set_status(StatusLevel::Success, message);
                        }
                        // MCP pending-write bookkeeping is a native-only feature
                        // (the `mcp_proposals` module is not built for wasm).
                        #[cfg(not(target_arch = "wasm32"))]
                        if last_error.is_some() {
                            this.complete_mcp_pending_write_if_tab(
                                &tab_id_for_pump,
                                last_error.clone(),
                            );
                        } else if !saw_cancel {
                            let note = affected_rows.map(|rows| {
                                crate::localization::format(
                                    if rows == 1 {
                                        "shell.execute.row_affected_note"
                                    } else {
                                        "shell.execute.rows_affected_note"
                                    },
                                    &[("count", &rows.to_string())],
                                )
                            });
                            this.complete_mcp_pending_write_if_tab(&tab_id_for_pump, note);
                        }
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    pub(super) fn execute_query_on_targets(
        &mut self,
        tab_idx: usize,
        target_ids: Vec<String>,
        origin: ExecuteOrigin,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.get(tab_idx).is_some_and(|tab| tab.snapshot_only) {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.execute.multi_target_snapshot_read_only"),
            );
            cx.notify();
            return;
        }
        if self
            .tabs
            .get(tab_idx)
            .is_some_and(|tab| tab.transaction_mode == crate::tabs::TransactionMode::Manual)
        {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.execute.multi_target_manual_unavailable"),
            );
            cx.notify();
            return;
        }
        if self
            .tabs
            .get(tab_idx)
            .is_some_and(|tab| tab.proc_mode.enabled)
        {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.execute.multi_target_procedure_unavailable"),
            );
            cx.notify();
            return;
        }

        let results_entity = match self.tabs.get(tab_idx) {
            Some(tab) => tab.results.clone(),
            None => return,
        };

        let store = ConnectionStore::global();
        let mut targets: Vec<ConnectionProfile> = Vec::new();
        let all_profiles: std::collections::HashMap<_, _> = store
            .list()
            .into_iter()
            .map(|p| (p.id.0.to_string(), p))
            .collect();
        for id in &target_ids {
            if let Some(profile) = all_profiles.get(id) {
                targets.push(profile.clone());
            }
        }
        let missing = target_ids.len().saturating_sub(targets.len());
        if missing > 0 {
            results_entity.update(cx, |r, cx| {
                r.push_message(
                    crate::localization::format(
                        if missing == 1 {
                            "shell.execute.unknown_connection_singular"
                        } else {
                            "shell.execute.unknown_connection_plural"
                        },
                        &[("count", &missing.to_string())],
                    ),
                    cx,
                );
            });
        }
        if targets.is_empty() {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.execute.no_valid_connections"),
            );
            cx.notify();
            return;
        }

        let fallback_db_type = targets.first().map(|p| p.database_type);
        let Some((sql, _)) = self.current_user_sql(tab_idx, &origin, fallback_db_type, cx) else {
            return;
        };

        let tenant_execute_block = self.active_profile.as_ref().and_then(|profile| {
            let editor = self.tabs.get(tab_idx)?.editor.clone();
            crate::tenant_scoping::execute_block_for_fail_closed(
                profile,
                editor.read(cx).validation_diagnostics(),
            )
        });
        let tab_id = self.tabs[tab_idx].id.clone();
        match multi_run_gate_decision(
            &sql,
            &origin,
            &tab_id,
            self.pending.mutation_review.as_ref(),
            &targets,
            tenant_execute_block.as_deref(),
            store,
            self.production_unlock.as_ref(),
            sqlly_intel::query::safety::Instant::now(),
        ) {
            MultiRunGate::Confirm {
                bottom_line,
                has_unbounded,
                ai_review_required,
                target_names,
            } => {
                self.mutation_review_generation += 1;
                let generation = self.mutation_review_generation;
                let n = target_names.len();
                self.pending.mutation_review = Some(PendingMutationReview {
                    sql: sql.clone(),
                    tab_id: self.tabs[tab_idx].id.clone(),
                    bottom_line: crate::localization::format(
                        "shell.execute.multi_target_bottom_line",
                        &[
                            ("bottom_line", &bottom_line),
                            ("count", &n.to_string()),
                            ("targets", &target_names.join(", ")),
                        ],
                    ),
                    has_unbounded,
                    ai_review_required,
                    production: false,
                    ai_explanation: None,
                    ai_generation: generation,
                    notebook_cell: None,
                    multi_targets: Some(
                        target_ids
                            .iter()
                            .filter_map(|s| uuid::Uuid::parse_str(s).ok())
                            .collect(),
                    ),
                });
                if ai_review_required {
                    self.set_status(
                        StatusLevel::Busy,
                        crate::localization::text(
                            "shell.execute.multi_target_generating_ai_review",
                        ),
                    );
                    self.spawn_ai_mutation_review(sql.clone(), generation, cx);
                } else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.execute.multi_target_confirm_mutation"),
                    );
                }
                cx.notify();
                return;
            }
            MultiRunGate::Refuse { message } => {
                self.set_status(StatusLevel::Error, message);
                cx.notify();
                return;
            }
            MultiRunGate::Proceed => {}
        }

        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.last_multi_target_ids = target_ids.clone();
        }
        crate::license::increment_query_count();
        if origin.reveals_results() {
            self.show_results();
        }

        let mut name_counts = std::collections::HashMap::<String, usize>::new();
        for p in &targets {
            *name_counts.entry(p.name.clone()).or_default() += 1;
        }
        let display_name = |p: &ConnectionProfile| -> String {
            let mut name = p.name.clone();
            if name_counts.get(&p.name).copied().unwrap_or(0) > 1
                && let Some(db) = p.database.as_deref().filter(|d| !d.is_empty())
            {
                name = format!("{name} / {db}");
            }
            name
        };

        let tab_lane_id = self.tabs.get(tab_idx).map(|tab| tab.id.clone());
        let now = sqlly_intel::query::safety::Instant::now();
        let mut specs = Vec::new();
        let mut services_list = Vec::new();
        let mut display_names = Vec::new();
        for (target_index, mut profile) in targets.into_iter().enumerate() {
            crate::mutation_review::apply_inherited_execution_policy(
                &mut profile,
                store,
                self.production_unlock.as_ref(),
                now,
            );
            let services = self.services_for_profile(&profile);
            services_list.push(services.clone());
            let label = display_name(&profile);
            display_names.push(label.clone());
            specs.push(sqlly_services::MultiTargetSpec {
                target_index,
                display_name: label,
                profile,
                executor: services.queries.clone(),
                session: tab_lane_id
                    .as_ref()
                    .map(|id| format!("{id}#{target_index}")),
            });
        }

        let first_db_type = specs
            .first()
            .map(|s| s.profile.database_type)
            .unwrap_or_default();
        let first_services = specs.first().map(|s| self.services_for_profile(&s.profile));
        let row_edit_block = self.row_edit_connection_block();
        let formula_variables = self
            .tabs
            .get(tab_idx)
            .map(|tab| {
                crate::computed_columns::formula_variables(
                    tab.editor.read(cx).text(),
                    &tab.query_params.values(),
                )
            })
            .unwrap_or_default();
        let run = results_entity.update(cx, |r, cx| {
            r.set_formula_variables(formula_variables);
            r.set_query_sql(&sql, cx);
            r.set_row_edit_connection_block(row_edit_block);
            r.set_source_db_type(first_db_type);
            if let Some(spec) = specs.first() {
                r.set_layout_scope(Some(crate::column_layout::LayoutScope::from_profile(
                    &spec.profile,
                )));
                r.set_fk_label_context(Some(spec.profile.clone()), first_services.clone());
                r.set_masking_overrides(spec.profile.masking.clone());
            }
            r.start_query(cx);
            r.run_generation()
        });
        self.set_status(
            StatusLevel::Busy,
            crate::localization::text("shell.execute.multi_target_query_running"),
        );
        self.pending.mutation_review = None;

        if let Some(prior) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|t| t.active_query_id.take())
            && let Some(first) = specs.first()
        {
            let services = self.services_for_profile(&first.profile);
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let _ = services.queries.cancel(&prior).await;
                    })
                    .await
                    .ok();
            })
            .detach();
        }
        if let Some(prior) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|t| t.active_multi_run.take())
        {
            let task_ids = prior.task_ids;
            let services = prior.services;
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let cancel_jobs: Vec<(usize, String)> = task_ids
                            .lock()
                            .unwrap()
                            .iter()
                            .enumerate()
                            .filter_map(|(i, id)| id.as_ref().map(|id| (i, id.clone())))
                            .collect();
                        for (i, id) in cancel_jobs {
                            if let Some(executor) = services.get(i) {
                                let _ = executor.queries.cancel(&id).await;
                            }
                        }
                    })
                    .await
                    .ok();
            })
            .detach();
        }

        let row_buffer = 256usize;
        let history_targets: Vec<(String, Option<String>, String)> = specs
            .iter()
            .map(|s| {
                (
                    s.profile.server.clone(),
                    s.profile.database.clone(),
                    s.display_name.clone(),
                )
            })
            .collect();
        let (handle, driver) = sqlly_services::run_multi_target(specs, sql.clone(), row_buffer, 4);
        let task_ids = handle.task_ids.clone();
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.active_multi_run = Some(crate::tabs::ActiveMultiRun {
                task_ids: task_ids.clone(),
                services: services_list.clone(),
            });
        }

        cx.background_spawn(async move {
            crate::tokio_handle().spawn(driver).await.ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        let log = self.log.clone();
        let tab_id_for_pump = tab_id.clone();
        let history_sql = sql.clone();
        let mut events = handle.events;
        cx.spawn(async move |_this, _cx| {
            let started = std::time::Instant::now();
            let mut result_set_counts = vec![0usize; display_names.len()];
            let mut failures = 0usize;
            let mut first_error: Option<String> = None;
            let mut saw_cancel = false;
            while let Some(event) = events.recv().await {
                match event {
                    sqlly_services::MultiTargetEvent::Target {
                        target_index,
                        event,
                    } => {
                        if let sqlly_services::QueryEvent::ResultSet { index, .. } = &event {
                            let local = index % sqlly_services::INDEX_STRIDE;
                            let label = if local == 0 {
                                display_names.get(target_index).cloned().unwrap_or_else(|| {
                                    crate::localization::format(
                                        "shell.execute.target_fallback",
                                        &[("number", &(target_index + 1).to_string())],
                                    )
                                })
                            } else {
                                let base =
                                    display_names.get(target_index).cloned().unwrap_or_else(|| {
                                        crate::localization::text("shell.execute.target_name")
                                    });
                                crate::localization::format(
                                    "shell.execute.result_set_label",
                                    &[("name", &base), ("number", &(local + 1).to_string())],
                                )
                            };
                            let remapped = *index;
                            let mut cx = async_cx.clone();
                            if weak_self
                                .update(&mut cx, |this, cx| {
                                    if results_entity.read(cx).run_generation() != run {
                                        return;
                                    }
                                    results_entity.update(cx, |r, cx| {
                                        r.set_index_label(remapped, label, cx);
                                    });
                                    this.log_console.update(cx, |view, cx| view.refresh(cx));
                                })
                                .is_err()
                            {
                                break;
                            }
                            if let Some(slot) = result_set_counts.get_mut(target_index) {
                                *slot = slot.saturating_add(1);
                            }
                        }
                        if let sqlly_services::QueryEvent::Error { message } = &event {
                            let name =
                                display_names.get(target_index).cloned().unwrap_or_else(|| {
                                    crate::localization::text("shell.execute.target_name")
                                });
                            let msg = format!("[{name}] {message}");
                            log.log_error("query", &msg);
                            if first_error.is_none() {
                                first_error = Some(msg.clone());
                            }
                            let mut cx = async_cx.clone();
                            if weak_self
                                .update(&mut cx, |this, cx| {
                                    if results_entity.read(cx).run_generation() != run {
                                        return;
                                    }
                                    results_entity.update(cx, |r, cx| r.push_message(msg, cx));
                                    this.arm_firewall_prompt(message, None, cx);
                                    this.log_console.update(cx, |view, cx| view.refresh(cx));
                                })
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            let mut cx = async_cx.clone();
                            if weak_self
                                .update(&mut cx, |this, cx| {
                                    if results_entity.read(cx).run_generation() != run {
                                        return;
                                    }
                                    results_entity.update(cx, |r, cx| {
                                        r.handle_event_for_run(run, event, cx);
                                    });
                                    this.log_console.update(cx, |view, cx| view.refresh(cx));
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    sqlly_services::MultiTargetEvent::TargetFinished {
                        target_index,
                        outcome,
                    } => {
                        let name = display_names.get(target_index).cloned().unwrap_or_else(|| {
                            crate::localization::text("shell.execute.target_name")
                        });
                        let msg = match &outcome {
                            sqlly_services::TargetOutcome::Completed { elapsed_ms } => {
                                crate::localization::format(
                                    "shell.execute.target_completed",
                                    &[("name", &name), ("milliseconds", &elapsed_ms.to_string())],
                                )
                            }
                            sqlly_services::TargetOutcome::Failed { message } => {
                                failures += 1;
                                if first_error.is_none() {
                                    first_error = Some(format!("[{name}] {message}"));
                                }
                                format!("[{name}] {message}")
                            }
                            sqlly_services::TargetOutcome::Cancelled => {
                                saw_cancel = true;
                                crate::localization::format(
                                    "shell.execute.target_cancelled",
                                    &[("name", &name)],
                                )
                            }
                        };
                        let mut cx = async_cx.clone();
                        results_entity.update(&mut cx, |r, cx| r.push_message(msg.clone(), cx));
                        if let Some((server, database, _)) = history_targets.get(target_index) {
                            let history_outcome = match &outcome {
                                sqlly_services::TargetOutcome::Completed { .. } => {
                                    crate::query_history::HistoryOutcome::Succeeded
                                }
                                sqlly_services::TargetOutcome::Failed { .. } => {
                                    crate::query_history::HistoryOutcome::Failed
                                }
                                sqlly_services::TargetOutcome::Cancelled => {
                                    crate::query_history::HistoryOutcome::Cancelled
                                }
                            };
                            let _ = weak_self.update(&mut cx, |this, _cx| {
                                this.query_history
                                    .log(&crate::query_history::HistoryRecord {
                                        sql: &history_sql,
                                        server,
                                        database: database.as_deref(),
                                        duration_ms: None,
                                        outcome: history_outcome,
                                        provenance: crate::query_history::HistoryProvenance::Manual,
                                        affected_rows: None,
                                        rollback_sql: None,
                                    });
                            });
                        }
                    }
                    sqlly_services::MultiTargetEvent::TargetStarted { .. } => {}
                    sqlly_services::MultiTargetEvent::AllFinished { any_cancelled, .. } => {
                        saw_cancel |= any_cancelled;
                        break;
                    }
                }
            }
            let n = display_names.len();
            let mut cx = async_cx.clone();
            let _ = weak_self.update(&mut cx, |this, cx| {
                if results_entity.read(cx).run_generation() != run {
                    return;
                }
                if let Some(tab_idx) = this.tab_index_by_id(&tab_id_for_pump)
                    && let Some(tab) = this.tabs.get_mut(tab_idx)
                {
                    tab.active_multi_run = None;
                }
                // The runner swallows each target's terminal event (it turns
                // them into `TargetFinished`), so end the run on the results
                // view here — otherwise its footer keeps counting "Running"
                // and pivot/chart stay "Loading…" after every target is done.
                results_entity.update(cx, |r, cx| {
                    if r.is_running() {
                        let terminal = if saw_cancel {
                            sqlly_services::QueryEvent::Cancelled
                        } else {
                            sqlly_services::QueryEvent::Completed {
                                elapsed_ms: started.elapsed().as_millis(),
                            }
                        };
                        r.handle_event_for_run(run, terminal, cx);
                    }
                });
                let status = if failures > 0 {
                    StatusLevel::Error
                } else if saw_cancel {
                    StatusLevel::Info
                } else {
                    StatusLevel::Success
                };
                let message = if saw_cancel && failures == 0 {
                    crate::localization::text("shell.execute.multi_target_query_cancelled")
                } else {
                    let key = match (n, failures) {
                        (1, 0) => "shell.execute.summary_one_success",
                        (1, 1) => "shell.execute.summary_one_failure",
                        (_, 0) => "shell.execute.summary_many_success",
                        (_, 1) => "shell.execute.summary_many_one_failure",
                        _ => "shell.execute.summary_many_failures",
                    };
                    match (n, failures) {
                        (1, 0 | 1) => crate::localization::text(key),
                        (_, 0 | 1) => {
                            crate::localization::format(key, &[("count", &n.to_string())])
                        }
                        _ => crate::localization::format(
                            key,
                            &[
                                ("count", &n.to_string()),
                                ("failures", &failures.to_string()),
                            ],
                        ),
                    }
                };
                if failures > 0
                    && let Some(err) = &first_error
                {
                    this.notifications.push(
                        crate::notification_center::NotificationKind::QueryFailed,
                        err.clone(),
                        chrono::Utc::now(),
                    );
                    this.last_query_error = Some(crate::app::ai::LastQueryError {
                        error: err.clone(),
                        sql: history_sql.clone(),
                        tab_id: tab_id_for_pump.clone(),
                    });
                }
                this.set_status(status, message);
                cx.notify();
            });
        })
        .detach();
    }

    #[allow(dead_code)]
    fn cancel_query(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_query_no_window(cx);
    }

    pub(super) fn set_active_auto_refresh(&mut self, secs: Option<u64>, cx: &mut Context<Self>) {
        let results = self.tabs.get_mut(self.active_tab).map(|tab| {
            tab.auto_refresh_secs = secs;
            tab.results.clone()
        });
        if let Some(results) = results {
            results.update(cx, |view, cx| view.set_auto_refresh_secs(secs, cx));
        }
        self.ensure_auto_refresh_ticker(cx);
        cx.notify();
    }

    pub(super) fn clear_auto_refresh_on_disconnect(&mut self, cx: &mut Context<Self>) {
        let views: Vec<_> = self
            .tabs
            .iter_mut()
            .map(|tab| {
                tab.auto_refresh_secs = None;
                tab.results.clone()
            })
            .collect();
        for results in views {
            results.update(cx, |view, cx| view.set_auto_refresh_secs(None, cx));
        }
        self.auto_refresh_ticker = None;
    }

    pub(super) fn spawn_total_count(
        &mut self,
        tab_idx: usize,
        slot: usize,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let tab_id = tab.id.clone();
        let results = tab.results.clone();
        let Some(profile) = self.active_profile.clone() else {
            results.update(cx, |view, cx| {
                view.set_total_row_count(
                    slot,
                    crate::results::TotalCount::Failed(crate::localization::text(
                        "shell.execute.not_connected",
                    )),
                    cx,
                );
            });
            return;
        };
        if self
            .pending
            .mutation_review
            .as_ref()
            .is_some_and(|review| review.tab_id == tab_id)
        {
            results.update(cx, |view, cx| {
                view.set_total_row_count(
                    slot,
                    crate::results::TotalCount::Failed(crate::localization::text(
                        "shell.execute.count_review_pending",
                    )),
                    cx,
                );
            });
            return;
        }
        let wrapped = match crate::count_query::count_wrapper_sql(profile.database_type, &sql) {
            Ok(sql) => sql,
            Err(err) => {
                results.update(cx, |view, cx| {
                    view.set_total_row_count(
                        slot,
                        crate::results::TotalCount::Failed(count_wrap_error_message(err)),
                        cx,
                    );
                });
                return;
            }
        };
        if let Some(prior) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|tab| tab.total_count_query_id.take())
        {
            let services = self.services_for_profile(&profile);
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let _ = services.queries.cancel(&prior).await;
                    })
                    .await
                    .ok();
            })
            .detach();
        }
        results.update(cx, |view, cx| {
            view.set_total_row_count(slot, crate::results::TotalCount::Pending, cx);
        });
        let services = self.services_for_profile(&profile);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<sqlly_services::QueryEvent>(16);
        let (id_tx, id_rx) = tokio::sync::oneshot::channel::<String>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    match services
                        .queries
                        .execute_in_session(&profile, None, &wrapped, 8)
                        .await
                    {
                        Ok(handle) => {
                            let _ = id_tx.send(handle.task_id.clone());
                            let mut events = handle.events;
                            while let Some(event) = events.recv().await {
                                if tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(sqlly_services::QueryEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                        }
                    }
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let mut task_id: Option<String> = None;
            if let Ok(id) = id_rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, _cx| {
                        if let Some(idx) = this.tab_index_by_id(&tab_id)
                            && let Some(tab) = this.tabs.get_mut(idx)
                        {
                            tab.total_count_query_id = Some(id.clone());
                        }
                    })
                    .ok();
                task_id = Some(id);
            }
            let mut parsed: Option<u64> = None;
            let mut failed: Option<String> = None;
            let mut cancelled = false;
            while let Some(event) = rx.recv().await {
                if parsed.is_none()
                    && let Some(n) = parse_count_from_event(&event)
                {
                    parsed = Some(n);
                }
                match event {
                    sqlly_services::QueryEvent::Error { message } => {
                        failed = Some(message);
                    }
                    sqlly_services::QueryEvent::Cancelled => {
                        cancelled = true;
                    }
                    _ => {}
                }
            }
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    let Some(idx) = this.tab_index_by_id(&tab_id) else {
                        return;
                    };
                    if let Some(tab) = this.tabs.get_mut(idx) {
                        if task_id
                            .as_ref()
                            .is_some_and(|id| tab.total_count_query_id.as_ref() != Some(id))
                        {
                            return;
                        }
                        tab.total_count_query_id = None;
                    }
                    let results = this.tabs.get(idx).map(|tab| tab.results.clone());
                    let Some(results) = results else {
                        return;
                    };
                    results.update(cx, |view, cx| {
                        if cancelled && parsed.is_none() && failed.is_none() {
                            view.clear_total_row_count(slot, cx);
                            return;
                        }
                        if let Some(message) = failed {
                            view.set_total_row_count(
                                slot,
                                crate::results::TotalCount::Failed(message),
                                cx,
                            );
                        } else if let Some(n) = parsed {
                            view.set_total_row_count(
                                slot,
                                crate::results::TotalCount::Ready(n),
                                cx,
                            );
                        } else {
                            view.set_total_row_count(
                                slot,
                                crate::results::TotalCount::Failed(crate::localization::text(
                                    "shell.execute.no_count_returned",
                                )),
                                cx,
                            );
                        }
                    });
                })
                .ok();
        })
        .detach();
    }

    pub(super) fn cancel_total_count(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return;
        };
        let task_id = tab.total_count_query_id.take();
        let results = tab.results.clone();
        results.update(cx, |view, cx| view.clear_pending_total_counts(cx));
        let Some(task_id) = task_id else {
            return;
        };
        let services = if let Some(profile) = self.active_profile.as_ref() {
            self.services_for_profile(profile)
        } else {
            self.services.clone()
        };
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let _ = services.queries.cancel(&task_id).await;
                })
                .await
                .ok();
        })
        .detach();
    }

    pub(super) fn ensure_auto_refresh_ticker(&mut self, cx: &mut Context<Self>) {
        if self.auto_refresh_ticker.is_some() {
            return;
        }
        if !self.tabs.iter().any(|tab| tab.auto_refresh_secs.is_some()) {
            return;
        }
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        self.auto_refresh_ticker = Some(cx.spawn(async move |_this, _cx| {
            loop {
                async_cx
                    .background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let mut cx = async_cx.clone();
                match weak.update(&mut cx, |this, cx| this.auto_refresh_tick(cx)) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        }));
    }

    fn auto_refresh_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.tabs.iter().any(|tab| tab.auto_refresh_secs.is_some()) {
            self.auto_refresh_ticker = None;
            return false;
        }
        let idx = self.active_tab;
        let Some(tab) = self.tabs.get(idx) else {
            return true;
        };
        let input = crate::auto_refresh::TickInput {
            interval_secs: tab.auto_refresh_secs,
            tab_is_active: true,
            run_in_flight: tab.active_query_id.is_some() || tab.results.read(cx).is_running(),
            mutation_review_pending: self
                .pending
                .mutation_review
                .as_ref()
                .is_some_and(|review| review.tab_id == tab.id),
            tab_snapshot_only: tab.snapshot_only,
            last_sql_is_select_only: tab.last_run_sql.as_deref().is_some_and(|sql| {
                match self.active_profile.as_ref().map(|p| p.database_type) {
                    Some(db) if db.is_command_engine() => {
                        !sqlly_intel::query::safety::redis::has_write_command(sql)
                    }
                    Some(db) => {
                        sqlly_intel::query::safety::classify_select_only_for(db, sql).select_only
                    }
                    None => sqlly_intel::query::safety::classify_select_only(sql).select_only,
                }
            }),
            secs_since_last_completion: tab.last_run_completed_at.map(|t| t.elapsed().as_secs()),
        };
        if crate::auto_refresh::tick_decision(input) == crate::auto_refresh::TickDecision::Refresh {
            self.execute_auto_refresh(idx, cx);
        }
        true
    }

    fn execute_auto_refresh(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(sql) = self
            .tabs
            .get(tab_idx)
            .and_then(|tab| tab.last_run_sql.clone())
        else {
            return;
        };
        self.execute_query_with_origin(tab_idx, ExecuteOrigin::AutoRefresh { sql }, cx);
    }
}

#[cfg(test)]
mod run_gate_tests {
    use super::{
        ExecuteOrigin, PendingMutationReview, RunGateDecision, run_gate_decision,
        sql_for_execute_origin,
    };
    use sqlly_core::{ConnectionEnvironment, SecretProvider, SecretRef};
    use sqlly_intel::query::safety::{UnlockWindow, WriteUnlock};

    fn now() -> std::time::Instant {
        std::time::Instant::now()
    }

    fn profile() -> sqlly_core::ConnectionProfile {
        sqlly_core::ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "password".to_string(),
            },
        )
    }

    fn empty_store() -> crate::connection::ConnectionStore {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"))
    }

    fn production_store() -> crate::connection::ConnectionStore {
        let store = empty_store();
        let mut production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .expect("built-in Production environment");
        production.safety.is_production = true;
        store.upsert_environment(production);
        store
    }

    fn production_profile() -> sqlly_core::ConnectionProfile {
        let mut p = profile();
        p.environment = ConnectionEnvironment::Production;
        p
    }

    fn pending_review(tab_id: &str, sql: &str) -> PendingMutationReview {
        PendingMutationReview {
            sql: sql.to_string(),
            tab_id: tab_id.to_string(),
            bottom_line: "delete".to_string(),
            has_unbounded: false,
            ai_review_required: false,
            production: false,
            ai_explanation: None,
            ai_generation: 1,
            notebook_cell: None,
            multi_targets: None,
        }
    }

    #[test]
    fn user_run_blocks_while_review_pending_on_same_tab() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        let review = pending_review("tab-a", "DELETE FROM t");
        assert_eq!(
            run_gate_decision(
                "DELETE FROM t",
                &ExecuteOrigin::UserRun,
                "tab-a",
                Some(&review),
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::BlockedPendingReview
        );
    }

    #[test]
    fn auto_refresh_blocked_when_not_select_only() {
        let p = profile();
        let store = empty_store();
        assert_eq!(
            run_gate_decision(
                "DELETE FROM t",
                &ExecuteOrigin::AutoRefresh {
                    sql: "DELETE FROM t".to_string(),
                },
                "tab-a",
                None,
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::BlockAutoRefresh
        );
    }

    #[test]
    fn auto_refresh_blocked_while_review_pending() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        let review = pending_review("tab-a", "DELETE FROM t");
        assert_eq!(
            run_gate_decision(
                "SELECT 1",
                &ExecuteOrigin::AutoRefresh {
                    sql: "SELECT 1".to_string(),
                },
                "tab-a",
                Some(&review),
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::BlockedPendingReview
        );
    }

    #[test]
    fn user_run_on_other_tab_does_not_bypass_confirmation() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        let review = pending_review("tab-a", "DELETE FROM t");
        match run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::UserRun,
            "tab-b",
            Some(&review),
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation { .. } => {}
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
    }

    #[test]
    fn mutation_confirm_uses_snapshotted_sql_not_buffer() {
        let buffer = "DELETE FROM edited";
        let review_sql = "DELETE FROM reviewed";
        assert_eq!(
            sql_for_execute_origin(
                &ExecuteOrigin::MutationConfirm {
                    sql: review_sql.to_string(),
                },
                buffer,
            ),
            review_sql
        );
    }

    #[test]
    fn notebook_cell_origin_uses_snapshotted_sql() {
        assert_eq!(
            sql_for_execute_origin(
                &ExecuteOrigin::NotebookCell {
                    cell_id: "cell_1".into(),
                    sql: "SELECT 1 AS x".into(),
                },
                "SELECT buffer",
            ),
            "SELECT 1 AS x"
        );
    }

    #[test]
    fn notebook_cell_origin_still_requires_confirmation_for_writes() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        match run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::NotebookCell {
                cell_id: "cell_1".into(),
                sql: "DELETE FROM t".into(),
            },
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation { .. } => {}
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
    }

    #[test]
    fn tenant_block_wins_on_mutation_confirm() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        match run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::MutationConfirm {
                sql: "DELETE FROM t".to_string(),
            },
            "tab-a",
            None,
            Some(&p),
            Some(
                "`orders` is tenant-scoped by `tenant_id`, but the statement has no `tenant_id` filter",
            ),
            &store,
            None,
            now(),
        ) {
            RunGateDecision::BlockTenant { .. } => {}
            other => panic!("expected BlockTenant, got {other:?}"),
        }
    }

    #[test]
    fn proc_header_parsed_still_confirms_mutations() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        match run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation { .. } => {}
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
    }

    #[test]
    fn required_tenant_block_refuses_proceed() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        match run_gate_decision(
            "SELECT 1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            Some(
                "`orders` is tenant-scoped by `tenant_id`, but the statement has no `tenant_id` filter",
            ),
            &store,
            None,
            now(),
        ) {
            RunGateDecision::BlockTenant { .. } => {}
            other => panic!("expected BlockTenant, got {other:?}"),
        }
    }

    #[test]
    fn no_active_profile_proceeds() {
        let store = empty_store();
        assert_eq!(
            run_gate_decision(
                "DELETE FROM t",
                &ExecuteOrigin::UserRun,
                "tab-a",
                None,
                None,
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::Proceed
        );
    }

    #[test]
    fn confirm_data_changes_requires_confirmation_for_delete() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        match run_gate_decision(
            "DELETE FROM t WHERE id = 1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation { has_unbounded, .. } => {
                assert!(!has_unbounded);
            }
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
        assert_eq!(
            run_gate_decision(
                "SELECT 1",
                &ExecuteOrigin::UserRun,
                "tab-a",
                None,
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::Proceed
        );
    }

    #[test]
    fn block_all_but_select_requires_confirmation_for_delete() {
        let mut p = profile();
        p.block_all_but_select = true;
        let store = empty_store();
        match run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation { .. } => {}
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
    }

    #[test]
    fn unbounded_update_proceeds_by_default() {
        // Unbounded-statement protections are opt-in: a fresh profile allows
        // an UPDATE with no WHERE clause straight through.
        let p = profile();
        let store = empty_store();
        assert_eq!(
            run_gate_decision(
                "UPDATE t SET a = 1",
                &ExecuteOrigin::UserRun,
                "tab-a",
                None,
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::Proceed
        );
    }

    #[test]
    fn unbounded_update_confirms_when_protection_opted_in() {
        let mut p = profile();
        p.allow_unbounded_mutations = false;
        let store = empty_store();
        match run_gate_decision(
            "UPDATE t SET a = 1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation {
                has_unbounded: true,
                ..
            } => {}
            other => panic!("expected ConfirmMutation with unbounded, got {other:?}"),
        }
    }

    #[test]
    fn allow_unbounded_mutations_skips_confirmation() {
        let mut p = profile();
        p.allow_unbounded_mutations = true;
        let store = empty_store();
        assert_eq!(
            run_gate_decision(
                "UPDATE t SET a = 1",
                &ExecuteOrigin::UserRun,
                "tab-a",
                None,
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::Proceed
        );
    }

    #[test]
    fn entity_only_select_only_still_requires_confirmation_at_gate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        store.upsert_client(crate::connection::ClientEntity {
            id: uuid::Uuid::new_v4(),
            name: "Acme".to_string(),
            color_hex: None,
            image_path: None,
            safety: crate::connection::SafetyFeatures {
                only_allow_select: true,
                ..Default::default()
            },
        });
        let mut p = profile();
        p.client = Some("Acme".to_string());
        match run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation { .. } => {}
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
    }

    #[test]
    fn production_locked_blocks_write_at_gate() {
        let p = production_profile();
        let store = production_store();
        match run_gate_decision(
            "UPDATE t SET a=1 WHERE id=1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::BlockProductionReadOnly { .. } => {}
            other => panic!("expected BlockProductionReadOnly, got {other:?}"),
        }
    }

    #[test]
    fn production_locked_allows_select() {
        let p = production_profile();
        let store = production_store();
        assert_eq!(
            run_gate_decision(
                "SELECT 1",
                &ExecuteOrigin::UserRun,
                "tab-a",
                None,
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::Proceed
        );
    }

    #[test]
    fn production_unlocked_requires_confirmation_even_with_where() {
        let p = production_profile();
        let store = production_store();
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, now());
        match run_gate_decision(
            "UPDATE t SET a=1 WHERE id=1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            Some(&unlock),
            now(),
        ) {
            RunGateDecision::ConfirmMutation { production, .. } => {
                assert!(production);
            }
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
    }

    #[test]
    fn production_unlock_expired_blocks_again() {
        let p = production_profile();
        let store = production_store();
        let t0 = now();
        let unlock = WriteUnlock::start(UnlockWindow::OneMinute, t0);
        let expired = t0 + std::time::Duration::from_secs(61);
        match run_gate_decision(
            "UPDATE t SET a=1 WHERE id=1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            Some(&unlock),
            expired,
        ) {
            RunGateDecision::BlockProductionReadOnly { .. } => {}
            other => panic!("expected BlockProductionReadOnly, got {other:?}"),
        }
    }

    #[test]
    fn mutation_confirm_origin_still_proceeds_on_production() {
        let p = production_profile();
        let store = production_store();
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, now());
        assert_eq!(
            run_gate_decision(
                "UPDATE t SET a=1 WHERE id=1",
                &ExecuteOrigin::MutationConfirm {
                    sql: "UPDATE t SET a=1 WHERE id=1".to_string(),
                },
                "tab-a",
                None,
                Some(&p),
                None,
                &store,
                Some(&unlock),
                now(),
            ),
            RunGateDecision::Proceed
        );
    }

    #[test]
    fn tenant_block_wins_over_production() {
        let p = production_profile();
        let store = production_store();
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, now());
        match run_gate_decision(
            "UPDATE t SET a=1 WHERE id=1",
            &ExecuteOrigin::MutationConfirm {
                sql: "UPDATE t SET a=1 WHERE id=1".to_string(),
            },
            "tab-a",
            None,
            Some(&p),
            Some(
                "`orders` is tenant-scoped by `tenant_id`, but the statement has no `tenant_id` filter",
            ),
            &store,
            Some(&unlock),
            now(),
        ) {
            RunGateDecision::BlockTenant { .. } => {}
            other => panic!("expected BlockTenant, got {other:?}"),
        }
    }

    #[test]
    fn agent_grant_update_still_requires_mutation_confirmation() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let store = empty_store();
        match run_gate_decision(
            "UPDATE t SET a = 1 WHERE id = 1",
            &ExecuteOrigin::AgentGrant {
                sql: "UPDATE t SET a = 1 WHERE id = 1".into(),
            },
            "tab-a",
            None,
            Some(&p),
            None,
            &store,
            None,
            now(),
        ) {
            RunGateDecision::ConfirmMutation { .. } => {}
            other => panic!("expected ConfirmMutation, got {other:?}"),
        }
    }

    #[test]
    fn override_false_profile_on_production_env_is_not_gated() {
        let mut p = production_profile();
        p.production_override = Some(false);
        let store = production_store();
        assert_eq!(
            run_gate_decision(
                "UPDATE t SET a=1 WHERE id=1",
                &ExecuteOrigin::UserRun,
                "tab-a",
                None,
                Some(&p),
                None,
                &store,
                None,
                now(),
            ),
            RunGateDecision::Proceed
        );
    }

    #[test]
    fn multi_gate_refuses_empty_targets() {
        let store = empty_store();
        match super::multi_run_gate_decision(
            "SELECT 1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            &[],
            None,
            &store,
            None,
            now(),
        ) {
            super::MultiRunGate::Refuse { .. } => {}
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn multi_gate_single_select_on_dev_and_test_proceeds() {
        let mut dev = profile();
        dev.environment = ConnectionEnvironment::Dev;
        let mut test = profile();
        test.name = "test-db".into();
        test.environment = ConnectionEnvironment::Test;
        let store = empty_store();
        assert_eq!(
            super::multi_run_gate_decision(
                "SELECT 1",
                &ExecuteOrigin::UserRun,
                "tab-a",
                None,
                &[dev, test],
                None,
                &store,
                None,
                now(),
            ),
            super::MultiRunGate::Proceed
        );
    }

    #[test]
    fn multi_gate_delete_on_any_target_confirms_and_lists_names() {
        let mut a = profile();
        a.name = "alpha".into();
        a.confirm_data_changes = true;
        let mut b = profile();
        b.name = "beta".into();
        b.confirm_data_changes = true;
        let store = empty_store();
        match super::multi_run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            &[a, b],
            None,
            &store,
            None,
            now(),
        ) {
            super::MultiRunGate::Confirm { target_names, .. } => {
                assert_eq!(target_names, vec!["alpha", "beta"]);
            }
            other => panic!("expected Confirm, got {other:?}"),
        }
    }

    #[test]
    fn multi_gate_production_target_requires_confirm_for_data_change() {
        let mut prod = production_profile();
        prod.confirm_data_changes = true;
        let store = production_store();
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, now());
        match super::multi_run_gate_decision(
            "DELETE FROM t",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            &[prod],
            None,
            &store,
            Some(&unlock),
            now(),
        ) {
            super::MultiRunGate::Confirm { .. } => {}
            other => panic!("expected Confirm, got {other:?}"),
        }
    }

    #[test]
    fn multi_gate_tenant_block_refuses_whole_run() {
        let mut a = profile();
        a.name = "alpha".into();
        let mut b = profile();
        b.name = "beta".into();
        let store = empty_store();
        match super::multi_run_gate_decision(
            "SELECT 1",
            &ExecuteOrigin::UserRun,
            "tab-a",
            None,
            &[a, b],
            Some(
                "`orders` is tenant-scoped by `tenant_id`, but the statement has no `tenant_id` filter",
            ),
            &store,
            None,
            now(),
        ) {
            super::MultiRunGate::Refuse { .. } => {}
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn multi_gate_mutation_confirm_origin_proceeds() {
        let mut a = profile();
        a.confirm_data_changes = true;
        let store = empty_store();
        assert_eq!(
            super::multi_run_gate_decision(
                "DELETE FROM t",
                &ExecuteOrigin::MutationConfirm {
                    sql: "DELETE FROM t".into(),
                },
                "tab-a",
                None,
                &[a],
                None,
                &store,
                None,
                now(),
            ),
            super::MultiRunGate::Proceed
        );
    }
}

#[cfg(test)]
mod apply_include_statistics_tests {
    use super::apply_include_statistics;
    use crate::session_stats::{StatsDialect, wrap_mysql_session_stats, wrap_tds_session_stats};
    use sqlly_lang::sqlmodel::ConnectionKind;

    #[test]
    fn tds_select_is_wrapped_when_enabled() {
        let (sql, dialect) = apply_include_statistics("SELECT 1", ConnectionKind::Tds, true, false);
        assert_eq!(dialect, Some(StatsDialect::Tds));
        assert!(sql.contains("@@SPID"), "{sql}");
        assert!(sql.contains("SELECT 1"), "{sql}");
    }

    #[test]
    fn tds_batch_lead_ddl_is_not_wrapped() {
        let sql_text = "CREATE PROCEDURE dbo.p AS SELECT 1";
        let (sql, dialect) = apply_include_statistics(sql_text, ConnectionKind::Tds, true, false);
        assert_eq!(dialect, None);
        assert_eq!(sql, sql_text);
    }

    #[test]
    fn mysql_select_is_wrapped_when_enabled() {
        let (sql, dialect) =
            apply_include_statistics("SELECT 1", ConnectionKind::MySqlWire, true, false);
        assert_eq!(dialect, Some(StatsDialect::MySql));
        assert_eq!(sql, wrap_mysql_session_stats("SELECT 1"));
    }

    #[test]
    fn disabled_or_postgres_leaves_sql_unchanged() {
        let (sql, dialect) =
            apply_include_statistics("SELECT 1", ConnectionKind::Tds, false, false);
        assert_eq!(dialect, None);
        assert_eq!(sql, "SELECT 1");

        let (sql, dialect) =
            apply_include_statistics("SELECT 1", ConnectionKind::PostgresWire, true, false);
        assert_eq!(dialect, None);
        assert_eq!(sql, "SELECT 1");
    }

    #[test]
    fn tds_wrap_matches_session_stats_helper() {
        let (sql, _) = apply_include_statistics("SELECT 1", ConnectionKind::Tds, true, false);
        assert_eq!(sql, wrap_tds_session_stats("SELECT 1"));
    }

    #[test]
    fn proc_mode_active_skips_statistics_wrap() {
        let sql_text = "SELECT 1";
        let (sql, dialect) = apply_include_statistics(sql_text, ConnectionKind::Tds, true, true);
        assert_eq!(dialect, None);
        assert_eq!(sql, sql_text);
    }
}

#[cfg(test)]
mod execute_sql_resolution_tests {
    use super::{
        BufferSqlInput, ExecuteOrigin, apply_query_param_substitution, executed_buffer_span,
        resolve_buffer_sql, sql_for_execute_origin,
    };
    use sqlly_core::DatabaseType;

    #[test]
    fn user_runs_reveal_results_but_background_runs_do_not() {
        let sql = "SELECT 1".to_string();
        assert!(ExecuteOrigin::UserRun.reveals_results());
        assert!(ExecuteOrigin::AiRun.reveals_results());
        assert!(ExecuteOrigin::MutationConfirm { sql: sql.clone() }.reveals_results());
        assert!(ExecuteOrigin::AgentGrant { sql: sql.clone() }.reveals_results());
        assert!(!ExecuteOrigin::AutoRefresh { sql: sql.clone() }.reveals_results());
        assert!(
            !ExecuteOrigin::NotebookCell {
                cell_id: "c1".into(),
                sql,
            }
            .reveals_results()
        );
    }

    #[test]
    fn selection_nonempty_wins_over_buffer() {
        let buffer = "SELECT 1;\nSELECT 2;";
        let start = buffer.find("SELECT 2").unwrap();
        let (sql, region_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: Some(start..buffer.len()),
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(sql, "SELECT 2;");
        assert_eq!(region_start, start);
    }

    #[test]
    fn empty_selection_falls_through_to_buffer_or_statement() {
        let buffer = "SELECT 1;\nSELECT 2;";
        let (whole, whole_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: Some(0..0),
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(whole, buffer);
        assert_eq!(whole_start, 0);

        let cursor = buffer.find("SELECT 2").unwrap();
        let (statement, statement_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: None,
            buffer_text: buffer,
            cursor_offset: cursor,
            run_current_statement: true,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        // The dialect-aware splitter tiles the buffer and assigns the newline
        // after a terminator to the preceding statement, so the current
        // statement is "SELECT 2;" starting just past "SELECT 1;\n".
        assert_eq!(statement, "SELECT 2;");
        assert_eq!(statement_start, "SELECT 1;\n".len());
    }

    #[test]
    fn forced_statement_offset_wins_over_cursor() {
        let buffer = "SELECT 1;\nSELECT 2;";
        let start = buffer.find("SELECT 2").unwrap();
        let (sql, region_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: Some(0..buffer.len()),
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: Some(start),
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        let (expected_start, expected_end) =
            crate::mutation_review::statement_span_at(buffer, start);
        assert_eq!(sql, buffer[expected_start..expected_end]);
        assert_eq!(region_start, expected_start);
        assert!(sql.contains("SELECT 2"));
        assert!(!sql.contains("SELECT 1"));
    }

    #[test]
    fn executed_buffer_span_reports_selection_then_statement_then_whole() {
        let buffer = "SELECT 1;\nSELECT 2;";
        let sel_start = buffer.find("SELECT 2").unwrap();
        let selection = executed_buffer_span(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: Some(sel_start..buffer.len()),
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(selection, Some((sel_start, buffer.len())));

        let cursor = sel_start;
        let statement = executed_buffer_span(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: None,
            buffer_text: buffer,
            cursor_offset: cursor,
            run_current_statement: true,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(
            statement,
            Some(crate::mutation_review::statement_span_at(buffer, cursor))
        );

        let whole = executed_buffer_span(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: None,
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(whole, Some((0, buffer.len())));

        let confirm = executed_buffer_span(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::MutationConfirm {
                sql: "SELECT 1".into(),
            },
            proc_mode_active: false,
            selection_range: None,
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(confirm, None);
    }

    #[test]
    fn notebook_cell_resolve_buffer_sql_is_empty() {
        let (sql, start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::NotebookCell {
                cell_id: "cell_1".into(),
                sql: "SELECT 1".into(),
            },
            proc_mode_active: false,
            selection_range: None,
            buffer_text: "SELECT buffer",
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(sql, "");
        assert_eq!(start, 0);
    }

    #[test]
    fn proc_mode_ignores_selection() {
        let buffer = "CREATE PROC dbo.p AS SELECT 1";
        let start = buffer.find("SELECT 1").unwrap();
        let (sql, region_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: true,
            selection_range: Some(start..buffer.len()),
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(sql, buffer);
        assert_eq!(region_start, 0);
    }

    #[test]
    fn mutation_confirm_leaves_buffer_empty_for_origin_override() {
        let buffer = "DELETE FROM edited";
        let (resolved, region_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::MutationConfirm {
                sql: "DELETE FROM snapshotted".to_string(),
            },
            proc_mode_active: false,
            selection_range: None,
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert!(resolved.is_empty());
        assert_eq!(region_start, 0);
        assert_eq!(
            sql_for_execute_origin(
                &ExecuteOrigin::MutationConfirm {
                    sql: "DELETE FROM snapshotted".to_string(),
                },
                &resolved,
            ),
            "DELETE FROM snapshotted"
        );
    }

    #[test]
    fn auto_refresh_uses_remembered_sql_not_buffer() {
        let buffer = "SELECT 'from buffer'";
        let remembered = "SELECT 'from last run'";
        let (resolved, region_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::AutoRefresh {
                sql: remembered.to_string(),
            },
            proc_mode_active: false,
            selection_range: Some(0..buffer.len()),
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: false,
            forced_statement_offset: None,
            selected_batch: None,
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert!(resolved.is_empty());
        assert_eq!(region_start, 0);
        assert_eq!(
            sql_for_execute_origin(
                &ExecuteOrigin::AutoRefresh {
                    sql: remembered.to_string(),
                },
                &resolved,
            ),
            remembered
        );
        let substituted = apply_query_param_substitution(
            &ExecuteOrigin::AutoRefresh {
                sql: remembered.to_string(),
            },
            false,
            remembered,
            "-- sqlly param: @id int\nSELECT @id",
            DatabaseType::SqlServer,
            &[("@id".into(), "7".into())],
        )
        .unwrap();
        assert_eq!(substituted, remembered);
    }

    #[test]
    fn param_substitution_applies_to_selection_using_full_buffer_specs() {
        let buffer = "-- sqlly param: @id int\nSELECT @id;\nSELECT 99;";
        let selection = "SELECT @id;";
        let out = apply_query_param_substitution(
            &ExecuteOrigin::UserRun,
            false,
            selection,
            buffer,
            DatabaseType::SqlServer,
            &[("@id".into(), "7".into())],
        )
        .unwrap();
        assert_eq!(out, "SELECT 7;");
    }

    #[test]
    fn param_substitution_skipped_in_proc_mode_and_mutation_confirm() {
        let buffer = "-- sqlly param: @id int\nSELECT @id";
        let proc_sql = apply_query_param_substitution(
            &ExecuteOrigin::UserRun,
            true,
            "SELECT @id",
            buffer,
            DatabaseType::SqlServer,
            &[("@id".into(), "7".into())],
        )
        .unwrap();
        assert_eq!(proc_sql, "SELECT @id");

        let confirm_sql = apply_query_param_substitution(
            &ExecuteOrigin::MutationConfirm {
                sql: "SELECT @id".to_string(),
            },
            false,
            "SELECT @id",
            buffer,
            DatabaseType::SqlServer,
            &[],
        )
        .unwrap();
        assert_eq!(confirm_sql, "SELECT @id");
    }

    #[test]
    fn cancel_clears_proc_running_flag() {
        use super::take_proc_mode_running;
        let mut running = true;
        assert!(take_proc_mode_running(&mut running));
        assert!(!running);
        assert!(!take_proc_mode_running(&mut running));
    }

    #[test]
    fn selected_batch_beats_selection_and_toggle() {
        let buffer = "SELECT 1;\nSELECT 2;";
        let batch = "SELECT 2;";
        let start = buffer.find(batch).unwrap();
        let (sql, region_start) = resolve_buffer_sql(&BufferSqlInput {
            command_lines: false,
            origin: &ExecuteOrigin::UserRun,
            proc_mode_active: false,
            selection_range: Some(0..buffer.len()),
            buffer_text: buffer,
            cursor_offset: 0,
            run_current_statement: true,
            forced_statement_offset: None,
            selected_batch: Some((batch, start)),
            dialect: sqlly_lang::sqlmodel::Dialect::SqlServer,
        });
        assert_eq!(sql, batch);
        assert_eq!(region_start, start);
    }
}

#[cfg(test)]
mod run_label_tests {
    use super::{RunLabelInput, run_label};

    #[test]
    fn label_cancel_wins() {
        let (text, _) = run_label(RunLabelInput {
            running: true,
            proc_mode: false,
            has_selection: true,
            run_current_statement: true,
            statement_count: 5,
            file_backed: true,
        });
        assert_eq!(text, "Cancel");
    }

    #[test]
    fn label_selection_beats_statement() {
        let (text, _) = run_label(RunLabelInput {
            running: false,
            proc_mode: false,
            has_selection: true,
            run_current_statement: true,
            statement_count: 3,
            file_backed: false,
        });
        assert_eq!(text, "Run selection");
    }

    #[test]
    fn label_all_counts_statements() {
        let (text, tip) = run_label(RunLabelInput {
            running: false,
            proc_mode: false,
            has_selection: false,
            run_current_statement: false,
            statement_count: 4,
            file_backed: false,
        });
        assert_eq!(text, "Run all (4)");
        assert!(tip.contains('4'));
    }

    #[test]
    fn label_file() {
        let (text, _) = run_label(RunLabelInput {
            running: false,
            proc_mode: false,
            has_selection: false,
            run_current_statement: false,
            statement_count: 1,
            file_backed: true,
        });
        assert_eq!(text, "Run file");
    }

    #[test]
    fn label_single_statement_is_run() {
        let (text, _) = run_label(RunLabelInput {
            running: false,
            proc_mode: false,
            has_selection: false,
            run_current_statement: false,
            statement_count: 1,
            file_backed: false,
        });
        assert_eq!(text, "Run");
    }
}

impl AppView {
    pub(super) fn confirm_mutation(&mut self, cx: &mut Context<Self>) {
        // Leave the review in place while re-entering execute_query: its
        // confirmation gate only fires when `pending.mutation_review` is
        // None, so taking it here would re-analyze the SQL and pop the same
        // dialog again (Execute would never actually execute). The review is
        // cleared in spawn_query_execution once the query is really running.
        if let Some(review) = self.pending.mutation_review.clone() {
            // Fail closed: a required review that has not been generated yet
            // (or failed — which dismisses the dialog) can never be confirmed
            // past. The Execute button is disabled in that state too; this
            // guards the keyboard/palette path.
            if review.ai_review_required && review.ai_explanation.is_none() {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.execute.waiting_for_required_ai_review"),
                );
                cx.notify();
                return;
            }
            self.log.log_error(
                "mutation",
                &format!("User confirmed mutation: {}", review.bottom_line),
            );
            if let Some(tab_idx) = self.tab_index_by_id(&review.tab_id) {
                if let Some(ids) = review.multi_targets.clone() {
                    let target_ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
                    self.execute_query_on_targets(
                        tab_idx,
                        target_ids,
                        ExecuteOrigin::MutationConfirm {
                            sql: review.sql.clone(),
                        },
                        cx,
                    );
                } else {
                    self.execute_query_with_origin(
                        tab_idx,
                        ExecuteOrigin::MutationConfirm {
                            sql: review.sql.clone(),
                        },
                        cx,
                    );
                }
            }
        }
    }

    /// Generate the ADR-008 AI review for an armed mutation dialog on the
    /// local AI runtime. Fail-closed: any failure (unreachable runtime, bad
    /// model, timeout, empty review) dismisses the dialog and the query does
    /// not run. Stale completions (dialog cancelled or re-armed) are dropped
    /// by the generation token.
    pub(super) fn spawn_ai_mutation_review(
        &mut self,
        sql: String,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let model = self.ai_summary_model.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = crate::ai_review::generate_review("", &model, &sql).await;
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else { return };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    let still_mutation = this
                        .pending
                        .mutation_review
                        .as_ref()
                        .is_some_and(|r| r.ai_generation == generation);
                    let still_staged = this
                        .pending
                        .staged_save
                        .as_ref()
                        .is_some_and(|r| r.ai_generation == generation);
                    if !still_mutation && !still_staged {
                        return;
                    }
                    match result {
                        Ok(explanation) => {
                            if still_mutation
                                && let Some(review) = this.pending.mutation_review.as_mut()
                            {
                                review.ai_explanation = Some(explanation.clone());
                            }
                            if still_staged {
                                if let Some(save) = this.pending.staged_save.as_mut() {
                                    save.ai_explanation = Some(explanation);
                                }
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::text("shell.execute.ai_review_ready_save"),
                                );
                            } else {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::text("shell.execute.ai_review_ready_run"),
                                );
                            }
                        }
                        Err(e) => {
                            if still_mutation {
                                this.pending.mutation_review = None;
                            }
                            if still_staged {
                                this.pending.staged_save = None;
                            }
                            this.set_status(
                                StatusLevel::Error,
                                crate::localization::format(
                                    "shell.execute.ai_review_unavailable",
                                    &[("error", &e)],
                                ),
                            );
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    pub(super) fn cancel_mutation(&mut self, cx: &mut Context<Self>) {
        let tab_id = self
            .pending
            .mutation_review
            .as_ref()
            .map(|review| review.tab_id.clone());
        // MCP pending-write rejection is native-only (`mcp_proposals`).
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(tab_id) = tab_id {
            self.reject_mcp_pending_write_if_tab(&tab_id, "user declined");
        }
        self.pending.mutation_review = None;
        // A bypass armed for this run must not survive a cancel and silently
        // disable the rollback wrap on the next, unrelated run.
        self.execute_rollback_bypass_once = false;
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.execute.mutation_cancelled"),
        );
        cx.notify();
    }
}

/// One-line status-bar summary of a finished script run, e.g.
/// `"1 statement · 42 rows · 0.12 s"` or
/// `"3 statements · 128 rows affected · 0.42 s"`. `rows_affected` is `None`
/// when no DML ran; a zero `rows_returned` with no affected count omits the
/// rows part entirely rather than reporting "0 rows".
pub fn run_summary(
    statements: usize,
    rows_affected: Option<u64>,
    rows_returned: usize,
    elapsed_ms: u64,
) -> String {
    let mut parts = vec![crate::localization::format(
        if statements == 1 {
            "shell.execute.summary_statement"
        } else {
            "shell.execute.summary_statements"
        },
        &[("count", &statements.to_string())],
    )];
    if rows_returned > 0 {
        parts.push(crate::localization::format(
            if rows_returned == 1 {
                "shell.execute.summary_row"
            } else {
                "shell.execute.summary_rows"
            },
            &[("count", &rows_returned.to_string())],
        ));
    }
    if let Some(affected) = rows_affected {
        parts.push(crate::localization::format(
            if affected == 1 {
                "shell.execute.summary_row_affected"
            } else {
                "shell.execute.summary_rows_affected"
            },
            &[("count", &affected.to_string())],
        ));
    }
    parts.push(crate::localization::format(
        "shell.execute.summary_seconds",
        &[("seconds", &format!("{:.2}", elapsed_ms as f64 / 1000.0))],
    ));
    parts.join(" · ")
}

#[cfg(test)]
mod run_summary_tests {
    use super::run_summary;

    #[test]
    fn run_summary_singular_plural() {
        assert_eq!(
            run_summary(1, None, 42, 120),
            "1 statement · 42 rows · 0.12 s"
        );
        assert_eq!(
            run_summary(3, Some(128), 0, 420),
            "3 statements · 128 rows affected · 0.42 s"
        );
        assert_eq!(
            run_summary(1, Some(1), 1, 1000),
            "1 statement · 1 row · 1 row affected · 1.00 s"
        );
    }

    #[test]
    fn run_summary_omits_missing_parts() {
        // No result sets and no DML: statements and time only, never "0 rows".
        assert_eq!(run_summary(2, None, 0, 50), "2 statements · 0.05 s");
        // Both parts appear when both exist.
        assert_eq!(
            run_summary(2, Some(7), 10, 2500),
            "2 statements · 10 rows · 7 rows affected · 2.50 s"
        );
    }
}

/// The `with_entra_sql_token` continuation in `execute_query_with_origin`
/// captures the tab's stable id and re-resolves the index once the token
/// arrives. Driving the async mint itself needs a fake token provider harness
/// that doesn't exist here; the part that breaks — resolving a captured id
/// after tabs changed — is pure, so it is pinned directly. The async path was
/// verified by review.
#[cfg(test)]
mod token_mint_retargeting_tests {
    use super::super::tab_index_of;

    #[test]
    fn tab_closed_during_token_mint_resolves_to_none() {
        // Tab "b" started a run, then was closed while the Entra/cloud-IAM
        // token round-trip was in flight. The continuation must abort, not run
        // on whichever tab now occupies the captured index.
        let before: [&str; 3] = ["a", "b", "c"];
        let after_close: [&str; 2] = ["a", "c"];
        assert_eq!(tab_index_of(before.iter().copied(), "b"), Some(1));
        assert_eq!(tab_index_of(after_close.iter().copied(), "b"), None);
    }

    #[test]
    fn tabs_changed_during_token_mint_still_resolve_to_the_original_tab() {
        // A tab opened/closed/reordered during the mint shifts indices; the id
        // still finds the originating tab at its new position.
        let after_changes: [&str; 3] = ["new", "c", "b"];
        assert_eq!(tab_index_of(after_changes.iter().copied(), "b"), Some(2));
    }
}

#[cfg(test)]
mod database_change_retarget_tests {
    use super::database_change_retargets;

    #[test]
    fn a_name_differing_only_in_case_retargets() {
        // On a case-sensitive MySQL server these are two databases; the engine
        // reported the move, so the picker must follow it.
        assert!(database_change_retargets(
            Some("sqlly_pr1571"),
            "SQLLY_PR1571"
        ));
    }

    #[test]
    fn the_same_name_does_not_retarget() {
        assert!(!database_change_retargets(Some("sales"), "sales"));
    }

    #[test]
    fn a_different_name_or_no_current_database_retargets() {
        assert!(database_change_retargets(Some("sales"), "archive"));
        assert!(database_change_retargets(None, "sales"));
    }
}
