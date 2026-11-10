use super::*;

/// A failed query's error text together with the SQL that produced it, captured
/// at failure time so "Explain Last Error" always pairs matching halves.
#[derive(Clone, Debug)]
pub(crate) struct LastQueryError {
    pub error: String,
    pub sql: String,
    /// The tab whose run failed, so an error-fix conversation can be bound
    /// to it (and only it) when "Explain this error" starts.
    pub tab_id: String,
}

pub(crate) use crate::ai_panel::ErrorFixContext;

/// The fixed SQL, ready to drop into the failing statement's place: trimmed,
/// with one trailing terminator removed. The replacement range ends BEFORE
/// the original `;`, so keeping the AI's own terminator would double it.
/// Line endings are normalized to `\n` here, the same way the editor's
/// `replace_document` stores text, so offsets computed from the result (the
/// post-replace selection) match the buffer exactly.
pub(crate) fn fixed_sql_for_replacement(sql: &str) -> String {
    let sql = sql.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = sql.trim();
    trimmed
        .strip_suffix(';')
        .unwrap_or(trimmed)
        .trim_end()
        .to_string()
}

/// A statement's text for comparison: whitespace runs collapsed, one
/// trailing `;` dropped.
fn normalized_statement(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_suffix(';').unwrap_or(text);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the AI's fixed SQL is really a rewrite of the whole script rather
/// than just the failing statement: it holds several statements and either
/// repeats one of the script's OTHER statements or has as many statements as
/// the script. Pasting such an answer over one statement would duplicate the
/// rest of the script, so the caller widens the replacement to the whole run.
pub(crate) fn fix_rewrites_whole_run(
    run_sql: &str,
    statement_span: (usize, usize),
    fixed: &str,
    dialect: sqlly_lang::sqlmodel::Dialect,
) -> bool {
    let run_slices = sqlly_lang::sqllex::split_statements(run_sql, dialect);
    let fixed_statements: Vec<String> = sqlly_lang::sqllex::split_statements(fixed, dialect)
        .iter()
        .filter_map(|slice| fixed.get(slice.span.start..slice.span.end))
        .map(normalized_statement)
        .filter(|text| !text.is_empty())
        .collect();
    if run_slices.len() < 2 || fixed_statements.len() < 2 {
        return false;
    }
    let others: Vec<String> = run_slices
        .iter()
        .filter(|slice| slice.span.end <= statement_span.0 || slice.span.start >= statement_span.1)
        .filter_map(|slice| run_sql.get(slice.span.start..slice.span.end))
        .map(normalized_statement)
        .filter(|text| !text.is_empty())
        .collect();
    fixed_statements.len() >= run_slices.len()
        || others.iter().any(|other| fixed_statements.contains(other))
}

/// Where in `buffer` the recorded run text still lives, or None when the
/// buffer has drifted past an honest match. Precise first: the run region the
/// tab recorded at dispatch still holds the run text byte-for-byte. Then the
/// forgiving check: the run text occurs exactly once in the buffer (an edit
/// above it shifts offsets but cannot hide it). Anything else — the statement
/// itself edited, the same text duplicated, a rewritten buffer — is drift.
pub(crate) fn ai_fix_region_start(
    buffer: &str,
    run_sql: &str,
    last_executed_span: Option<(usize, usize)>,
) -> Option<usize> {
    if run_sql.trim().is_empty() {
        return None;
    }
    if let Some((start, end)) = last_executed_span
        && end <= buffer.len()
        && buffer.get(start..end) == Some(run_sql)
    {
        return Some(start);
    }
    let mut matches = buffer.match_indices(run_sql);
    match (matches.next(), matches.next()) {
        (Some((at, _)), None) => Some(at),
        _ => None,
    }
}

/// Byte range within `run_sql` of the statement that produced `error`: the
/// engine's reported error position (the same `execution_error::locate` parse
/// that drives the editor's error underline) expanded to the full statement
/// by the dialect splitter (the same splitter "Run current statement" uses).
/// `None` when the engine reported no usable position — guessing a statement
/// would be worse than the caller's insert-at-cursor fallback.
pub(crate) fn failing_statement_span(
    run_sql: &str,
    error: &str,
    db_type: sqlly_core::DatabaseType,
    auto_wrap_rollback: bool,
) -> Option<(usize, usize)> {
    let location = crate::execution_error::locate(&crate::execution_error::LocateInput {
        db_type,
        auto_wrap_rollback,
        run_sql,
        message: error,
    })?;
    statement_span_covering(run_sql, location.start, db_type.family())
}

/// The splitter slice holding byte offset `at` (a gap between statements —
/// whitespace or a comment — attaches to the nearest following statement,
/// then the previous one). Leading trivia of the chosen slice (blank lines,
/// line comments, block comments) is kept OUT of the returned span so
/// replacing it cannot silently eat the note above the statement.
fn statement_span_covering(
    sql: &str,
    at: usize,
    dialect: sqlly_lang::sqlmodel::Dialect,
) -> Option<(usize, usize)> {
    let slices = sqlly_lang::sqllex::split_statements(sql, dialect);
    if slices.is_empty() {
        return None;
    }
    let at = at.min(sql.len());
    let mut chosen = None;
    for slice in &slices {
        let (start, end) = (slice.span.start, slice.span.end);
        if (at >= start && at < end) || (at == end && end == sql.len()) || start > at {
            chosen = Some((start, end));
            break;
        }
    }
    let (start, end) = chosen.or_else(|| slices.last().map(|s| (s.span.start, s.span.end)))?;
    let start = trim_leading_trivia(sql, start, end);
    (start < end).then_some((start, end))
}

/// First byte at or after `start` that is not whitespace or a `--`/`/* */`
/// comment (an unterminated block comment leaves the span alone rather than
/// eating the buffer).
fn trim_leading_trivia(sql: &str, start: usize, end: usize) -> usize {
    let mut i = start.min(end);
    while i < end {
        let rest = &sql[i..end];
        if let Some(after) = rest.strip_prefix("--") {
            match after.find('\n') {
                Some(nl) => i += 2 + nl + 1,
                None => return end,
            }
        } else if let Some(after) = rest.strip_prefix("/*") {
            match after.find("*/") {
                Some(close) => i += 2 + close + 2,
                None => return start,
            }
        } else if rest.chars().next().is_some_and(char::is_whitespace) {
            i += rest.chars().next().map(char::len_utf8).unwrap_or(1);
        } else {
            break;
        }
    }
    i.min(end)
}

/// Whether the tab's connection wraps runs in a rollback transaction (it
/// shifts the engine's reported line numbers).
fn tab_auto_wrap(tab: &crate::tabs::QueryTab) -> bool {
    tab.connection_target
        .as_ref()
        .and_then(|target| crate::connection::ConnectionStore::global().get(target.profile_id))
        .map(|profile| profile.auto_wrap_rollback)
        .unwrap_or(false)
}

/// Where "Replace failing statement" puts `fixed` in `buffer`, composing the
/// drift check ([`ai_fix_region_start`]) with the statement location captured
/// in the context, and whether that is the whole run region (the answer
/// rewrote the whole script) rather than just the failing statement. Every
/// "can't honestly tell" returns None so the caller falls back to
/// insert-at-cursor with an honest note.
pub(crate) fn ai_fix_replacement_plan(
    buffer: &str,
    context: &ErrorFixContext,
    fixed: &str,
    db_type: sqlly_core::DatabaseType,
) -> Option<(usize, usize, bool)> {
    let region = ai_fix_region_start(buffer, &context.run_sql, context.last_executed_span)?;
    let (start, end) = context.statement_span?;
    if fix_rewrites_whole_run(&context.run_sql, (start, end), fixed, db_type.family()) {
        let run_end = region + context.run_sql.trim_end().trim_end_matches(';').len();
        return Some((region, run_end, true));
    }
    Some((region + start, region + end, false))
}

impl AppView {
    /// Whether FIM inline completion is live for `tab_idx`: the global
    /// preference switch AND the tab's toolbar sparkle toggle must both be on.
    pub(super) fn fim_effective_enabled(&self, tab_idx: usize) -> bool {
        self.fim_prefs.enabled
            && self
                .tabs
                .get(tab_idx)
                .is_some_and(|t| t.fim_autocomplete_enabled)
    }

    pub(super) fn schedule_fim_completion(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        if self.fim_prefs.trigger_kind == crate::fim_completion::FimTriggerKind::Manual {
            return;
        }
        let delay = self.fim_prefs.generation_delay_secs;
        self.schedule_fim_completion_with_delay(tab_idx, delay, cx);
    }

    /// Manual FIM request (the `TriggerAiCompletion` shortcut): fires
    /// immediately, in both Automatic and Manual trigger modes. Respects the
    /// same enablement gates as the automatic path.
    pub(super) fn trigger_fim_completion_manually(
        &mut self,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) {
        self.schedule_fim_completion_with_delay(tab_idx, 0.0, cx);
    }

    /// Schedule a debounced FIM (fill-in-the-middle) completion request for the
    /// editor in the given tab. Mirrors the Swift `scheduleFIMCompletion()`:
    /// cancel any pending request, increment the token, start a debounce
    /// timer, then fire the request.
    pub(super) fn schedule_fim_completion_with_delay(
        &mut self,
        tab_idx: usize,
        delay: f64,
        cx: &mut Context<Self>,
    ) {
        // Gate: FIM must be enabled globally and for this tab.
        if !self.fim_effective_enabled(tab_idx) {
            return;
        }
        // FIM operates on the focused VIEW: in a same-buffer split with the
        // second pane focused, ghost text belongs there, not in the
        // canonical editor.
        let editor = match self.focused_editor_for_tab(tab_idx) {
            Some(e) => e,
            None => return,
        };
        // Cancel any pending debounce.
        self.fim_debounce_task = None;
        // Hide any existing FIM suggestion — the prefix changed.
        editor.update(cx, |e, cx| e.hide_fim_suggestion(cx));
        // Gate: a cursor-anchored suggestion makes no sense over a selection,
        // and ghost text under the IntelliSense popup would fight it for the
        // same keystrokes (Swift `shouldRequestFIM` parity).
        {
            let e = editor.read(cx);
            if e.completion_popup_visible() || e.has_selection() {
                return;
            }
        }

        let token = self.fim_request_token + 1;
        self.fim_request_token = token;

        let prefs = self.fim_prefs.clone();
        let model = prefs.model;
        let weak_self = cx.weak_entity();
        let editor_entity = editor.clone();
        let async_cx = cx.to_async();

        self.fim_debounce_task = Some(cx.spawn(async move |_this, _cx| {
            // Debounce: wait for the configured delay before firing.
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_secs_f32(delay as f32))
                .await;
            // Snapshot the editor text + cursor at fire time, from the view
            // the request targeted — its cursor is the insertion point.
            let snapshot = {
                let mut cx = async_cx.clone();
                let editor_entity = editor_entity.clone();
                weak_self
                    .update(&mut cx, |_this, cx| {
                        let editor = editor_entity.read(cx);
                        let content = editor.text().to_string();
                        let cursor = editor.cursor();
                        Some((
                            content[..cursor.min(content.len())].to_string(),
                            content[cursor.min(content.len())..].to_string(),
                        ))
                    })
                    .ok()
                    .flatten()
            };
            let (prefix, suffix) = match snapshot {
                Some(s) => s,
                None => return,
            };
            // Gate: suppress if the prefix ends with a suppress token.
            if prefs.should_suppress(&prefix) {
                return;
            }
            // Gate: skip if the prefix is empty or just whitespace.
            if prefix.trim().is_empty() {
                return;
            }
            // Build the service from env + prefs and request completion.
            let service = crate::fim_completion::FimCompletionService::from_env(model, prefs);
            match service.complete(&prefix, &suffix).await {
                Ok(text) => {
                    // Drop any leading restatement of the text before the
                    // cursor — the base models sometimes re-emit the clause
                    // they just read in the prefix.
                    let text =
                        crate::fim_completion::trim_prefix_repetition(&prefix, text.trim_end());
                    if !text.trim().is_empty() {
                        // Only show if the token still matches (not cancelled).
                        let mut cx = async_cx.clone();
                        let still_valid = weak_self
                            .update(&mut cx, |this, _cx| this.fim_request_token == token)
                            .unwrap_or(false);
                        if still_valid {
                            let mut cx = async_cx.clone();
                            editor_entity.update(&mut cx, |e, cx| {
                                e.show_fim_suggestion(&text, cx);
                            });
                        }
                    }
                }
                Err(_) => {
                    let mut cx = async_cx.clone();
                    editor_entity.update(&mut cx, |e, cx| {
                        e.hide_fim_suggestion(cx);
                    });
                }
            }
        }));
    }

    /// Rough pixel width available to display the AI summary for `tab_idx`:
    /// the editor viewport width (the summary bar spans the same column)
    /// minus the "Update Summary" button and separators. `None` before the
    /// first layout, so callers fall back to the default word budget.
    pub(super) fn ai_summary_display_width(
        &self,
        tab_idx: usize,
        cx: &Context<Self>,
    ) -> Option<f32> {
        let editor_width = self.tabs.get(tab_idx)?.editor.read(cx).viewport_width();
        (editor_width > 0.0).then_some((editor_width - 160.0).max(0.0))
    }

    /// Recompute the ideal long-summary word count from the current query
    /// editor width. Called on every render, which includes every resize.
    /// Only the stored number changes — the next generation run uses the new
    /// budget; nothing is regenerated on the fly.
    pub(super) fn refresh_ai_summary_word_budget(&mut self, cx: &Context<Self>) {
        if let Some(width) = self.ai_summary_display_width(self.active_tab, cx) {
            self.ai_summary_long_word_budget =
                Some(crate::query_ai_summary::word_budget_for_width(width));
        }
    }

    /// Word budget for the next long-summary generation: the budget captured
    /// on the last layout, falling back to a fresh width measurement before
    /// the first render has stored one.
    pub(super) fn ai_summary_word_budget(
        &self,
        tab_idx: usize,
        cx: &Context<Self>,
    ) -> Option<usize> {
        self.ai_summary_long_word_budget.or_else(|| {
            self.ai_summary_display_width(tab_idx, cx)
                .map(crate::query_ai_summary::word_budget_for_width)
        })
    }

    /// Schedule a debounced AI summary refresh for the active tab's SQL,
    /// mirroring the Swift `QueryWorkspaceController.refreshDirtyAISummaries`.
    /// Uses a 5s debounce (vs. the Swift app's 120s/600s polling timers) — the
    /// debounce fires once after the user stops typing for 5 seconds.
    pub(super) fn schedule_ai_summary(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        if !self.ai_summary_enabled {
            return;
        }
        let tab = match self.tabs.get(tab_idx) {
            Some(t) => t,
            None => return,
        };
        // Gate: check which summaries can run (dirty + throttle).
        let now = Instant::now();
        let run_short = tab
            .ai_summary
            .can_run(crate::query_ai_summary::QueryAISummaryKind::Short, now);
        let run_long = tab
            .ai_summary
            .can_run(crate::query_ai_summary::QueryAISummaryKind::Long, now);
        if !run_short && !run_long {
            return;
        }

        // Cancel any pending debounce.
        self.ai_summary_debounce_task = None;

        let token = self.ai_summary_request_token + 1;
        self.ai_summary_request_token = token;

        let sql = tab.editor.read(cx).text().to_string();
        // Size the long description to the space that will display it: the
        // budget captured on the last layout (kept fresh across resizes).
        let word_budget = self.ai_summary_word_budget(tab_idx, cx);
        let model = self.ai_summary_model.clone();
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        let tab_id = tab.id.clone();

        self.ai_summary_debounce_task = Some(cx.spawn(async move |_this, _cx| {
            // Debounce: wait 5s before firing.
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_secs(5))
                .await;

            // Gate: skip if the SQL is empty or just whitespace.
            if sql.trim().is_empty() {
                return;
            }

            // Read OLLAMA_HOST env (same as FIM).
            let base_url = std::env::var("OLLAMA_HOST").unwrap_or_default();

            // Generate every dirty kind; a failed kind is silently skipped —
            // the summary bar keeps showing the last successful summary.
            let mut generated = Vec::new();
            for (run, kind) in [
                (
                    run_short,
                    crate::query_ai_summary::QueryAISummaryKind::Short,
                ),
                (run_long, crate::query_ai_summary::QueryAISummaryKind::Long),
            ] {
                if !run {
                    continue;
                }
                if let Ok(text) = crate::query_ai_summary::generate_summary_with_budget(
                    &sql,
                    kind,
                    &base_url,
                    &model,
                    word_budget,
                )
                .await
                {
                    generated.push((kind, text));
                }
            }
            if generated.is_empty() {
                return;
            }

            let mut cx = async_cx.clone();
            let still_valid = weak_self
                .update(&mut cx, |this, _cx| this.ai_summary_request_token == token)
                .unwrap_or(false);
            if still_valid {
                let mut cx = async_cx.clone();
                let _ = weak_self.update(&mut cx, |this, cx| {
                    // Compute the active connection before the mutable
                    // borrow of `this.tabs` to avoid borrow conflict.
                    let active_conn = this.active_connection_metadata();
                    if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                        for (kind, text) in generated {
                            tab.ai_summary.set_summary(kind, text, Instant::now());
                        }
                        // Flush to the metadata store (with resume +
                        // connection metadata). Use the tab's
                        // remembered connection, falling back to active.
                        let connection = tab.pending_resume_connection.clone().or(active_conn);
                        let resume = tab.current_resume_metadata();
                        let key = tab.metadata_key();
                        let record = tab
                            .ai_summary
                            .make_record_with(connection.as_ref(), resume.as_ref());
                        this.ai_summary_metadata.set(key, record);
                        if let Some(path) = crate::query_ai_summary::ai_summary_metadata_file() {
                            let _ = crate::query_ai_summary::QueryAISummaryMetadataStore::save(
                                &this.ai_summary_metadata,
                                &path,
                            );
                        }
                    }
                    cx.notify();
                });
            }
        }));
    }

    /// Force-refresh the AI summary for the active tab, bypassing the
    /// dirty/throttle gate. Mirrors the Swift
    /// `forceRefreshSummaries(for:)` flow.
    pub(super) fn force_ai_summary_refresh(&mut self, cx: &mut Context<Self>) {
        let tab_idx = self.active_tab;
        let sql = match self.tabs.get(tab_idx) {
            Some(t) => t.editor.read(cx).text().to_string(),
            None => return,
        };
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.ai_summary
                .mark_needs_refresh(crate::query_ai_summary::QueryAISummaryKind::Short);
            tab.ai_summary
                .mark_needs_refresh(crate::query_ai_summary::QueryAISummaryKind::Long);
        }

        let token = self.ai_summary_request_token + 1;
        self.ai_summary_request_token = token;

        let tab_id = match self.tabs.get(tab_idx) {
            Some(t) => t.id.clone(),
            None => return,
        };
        let word_budget = self.ai_summary_word_budget(tab_idx, cx);
        let model = self.ai_summary_model.clone();
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();

        cx.spawn(async move |_this, _cx| {
            if sql.trim().is_empty() {
                return;
            }
            let base_url = std::env::var("OLLAMA_HOST").unwrap_or_default();
            let mut generated = Vec::new();
            for kind in [
                crate::query_ai_summary::QueryAISummaryKind::Short,
                crate::query_ai_summary::QueryAISummaryKind::Long,
            ] {
                if let Ok(text) = crate::query_ai_summary::generate_summary_with_budget(
                    &sql,
                    kind,
                    &base_url,
                    &model,
                    word_budget,
                )
                .await
                {
                    generated.push((kind, text));
                }
            }
            if !generated.is_empty() {
                let mut cx = async_cx.clone();
                let _ = weak_self.update(&mut cx, |this, cx| {
                    let active_conn = this.active_connection_metadata();
                    if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                        for (kind, text) in generated {
                            tab.ai_summary.set_summary(kind, text, Instant::now());
                        }
                        let connection = tab.pending_resume_connection.clone().or(active_conn);
                        let resume = tab.current_resume_metadata();
                        let key = tab.metadata_key();
                        let record = tab
                            .ai_summary
                            .make_record_with(connection.as_ref(), resume.as_ref());
                        this.ai_summary_metadata.set(key, record);
                        if let Some(path) = crate::query_ai_summary::ai_summary_metadata_file() {
                            let _ = crate::query_ai_summary::QueryAISummaryMetadataStore::save(
                                &this.ai_summary_metadata,
                                &path,
                            );
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn handle_ai_event(
        &mut self,
        _panel: Entity<AiPanel>,
        event: &AiPanelEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AiPanelEvent::InsertSql(sql) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    // Insert at the cursor, which is what "Insert" says and what
                    // the user asked for. `set_text` replaced the whole document
                    // and cleared the undo stack, so accepting an AI suggestion
                    // silently destroyed whatever was already in the tab.
                    editor.update(cx, |e, cx| {
                        e.insert(sql, cx);
                    });
                }
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.ai.sql_inserted"),
                );
                cx.notify();
            }
            AiPanelEvent::ReplaceFailingStatement { sql, context } => {
                self.replace_failing_statement(sql.clone(), context.clone(), cx);
            }
            AiPanelEvent::OpenSqlInNewQuery(sql) => {
                self.new_tab(cx, Some(sql.clone()));
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.ai.sql_opened_new_query"),
                );
                cx.notify();
            }
            AiPanelEvent::OpenSqlAtOffset { sql, offset } => {
                self.new_tab(cx, Some(sql.clone()));
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    let offset = *offset;
                    editor.update(cx, |editor, cx| {
                        editor.adopt_caret_state(offset, None, cx);
                    });
                }
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.ai.sql_opened_flagged_offset"),
                );
                cx.notify();
            }
            AiPanelEvent::RunSql(sql) => {
                let proc_dialect = self.active_proc_dialect();
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.proc_mode.refresh_from_script_for(sql, proc_dialect);
                    let editor = tab.editor.clone();
                    editor.update(cx, |e, cx| {
                        e.set_text(sql, cx);
                    });
                    self.execute_query_with_origin(
                        self.active_tab,
                        super::execute::ExecuteOrigin::AiRun,
                        cx,
                    );
                }
            }
            AiPanelEvent::OpenInlineResultInTab { sql } => {
                self.new_tab(cx, Some(sql.clone()));
                self.execute_query_with_origin(
                    self.active_tab,
                    super::execute::ExecuteOrigin::AiRun,
                    cx,
                );
            }
            AiPanelEvent::InlineRunRequested { index, sql } => {
                self.sync_ai_inline_policy(cx);
                let production = self.profile_is_production();
                if let Some(message) =
                    self.inline_reads_blocked_message(self.active_profile.as_ref())
                {
                    self.set_status(StatusLevel::Error, message.clone());
                    self.ai_panel.update(cx, |panel, cx| {
                        panel.set_inline_policy(Some(message), production);
                        cx.notify();
                    });
                } else {
                    let index = *index;
                    let sql = sql.clone();
                    self.ai_panel.update(cx, |panel, cx| {
                        panel.set_inline_policy(None, production);
                        panel.start_inline_run(index, sql, cx);
                    });
                }
            }
            AiPanelEvent::ProposeWrite { sql } => {
                if let Some(profile) = self.active_profile.as_ref() {
                    self.agent_write_grant =
                        Some(crate::agent_grant::WriteGrant::new(sql.as_str(), profile));
                }
                self.new_tab(cx, Some(sql.clone()));
                self.execute_query_with_origin(
                    self.active_tab,
                    super::execute::ExecuteOrigin::AgentGrant { sql: sql.clone() },
                    cx,
                );
            }
            AiPanelEvent::RevealSchemaObject { schema, name } => {
                let server_id = self.active_profile.as_ref().map(|profile| profile.id.0);
                let revealed = self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.reveal_object(server_id, schema.as_deref(), name, cx)
                });
                if revealed {
                    self.sidebar_visible = true;
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format("shell.ai.object_revealed", &[("name", name)]),
                    );
                } else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format(
                            "shell.ai.object_not_loaded",
                            &[("name", name)],
                        ),
                    );
                }
                cx.notify();
            }
            AiPanelEvent::OpenSchemaObject { schema, name, kind } => {
                self.open_model_object_ddl(schema.clone(), name.clone(), *kind, cx)
            }
            AiPanelEvent::ActiveSectionChanged => self.save_workspace_session(cx),
        }
    }

    pub(super) fn handle_toggle_agent_mode(
        &mut self,
        _action: &ToggleAgentMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ai_panel
            .update(cx, |panel, cx| panel.toggle_agent_mode(cx));
    }

    pub(super) fn handle_toggle_ai_panel(
        &mut self,
        _action: &ToggleAiPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ai_visible {
            self.ai_panel
                .update(cx, |panel, cx| panel.dismiss_floating_ui(cx));
            self.restore_workbench_focus(Some(window), cx);
            self.ai_visible = false;
        } else {
            self.sync_ai_panel_context(cx);
            self.select_mid_region_tab(MidRegionTab::Ai, cx);
        }
        self.save_workspace_session(cx);
        cx.notify();
    }

    fn sync_ai_panel_context(&mut self, cx: &mut Context<Self>) {
        let selection = self
            .tabs
            .get(self.active_tab)
            .and_then(|tab| tab.editor.read(cx).selected_text())
            .filter(|text| !text.trim().is_empty());
        let root = self.project_file_picker_root.clone();
        self.sync_ai_inline_policy(cx);
        self.ai_panel.update(cx, |panel, _cx| {
            panel.set_editor_selection(selection);
            panel.set_project_root(root);
        });
    }

    fn sync_ai_inline_policy(&mut self, cx: &mut Context<Self>) {
        let blocked = self.inline_reads_blocked_message(self.active_profile.as_ref());
        let production = self.profile_is_production();
        self.ai_panel.update(cx, |panel, _cx| {
            panel.set_inline_policy(blocked, production);
        });
    }

    fn profile_is_production(&self) -> bool {
        self.active_profile.as_ref().is_some_and(|profile| {
            let store = crate::connection::ConnectionStore::global();
            crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store)
                .is_production
        })
    }

    /// Reuses [`super::execute::run_gate_decision`] so inline SELECTs honour
    /// the same production read-lock as the editor Run path, without editing
    /// execute.rs.
    fn inline_reads_blocked_message(
        &self,
        profile: Option<&sqlly_core::ConnectionProfile>,
    ) -> Option<String> {
        let store = crate::connection::ConnectionStore::global();
        match super::execute::run_gate_decision(
            "SELECT 1",
            &super::execute::ExecuteOrigin::AiRun,
            "",
            None,
            profile,
            None,
            store,
            self.production_unlock.as_ref(),
            sqlly_intel::query::safety::Instant::now(),
        ) {
            super::execute::RunGateDecision::BlockProductionReadOnly { message } => Some(message),
            _ => None,
        }
    }

    pub(super) fn handle_generate_sql(
        &mut self,
        _action: &GenerateSql,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_ai_panel_context(cx);
        self.ai_panel.update(cx, |p, cx| {
            p.set_active_section(AiSection::GenerateSql, cx);
        });
        self.ai_visible = true;
        cx.notify();
    }

    pub(super) fn handle_ask_schema(
        &mut self,
        _action: &AskSchema,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_ai_panel_context(cx);
        self.ai_panel.update(cx, |p, cx| {
            p.set_active_section(AiSection::AskSchema, cx);
        });
        self.ai_visible = true;
        cx.notify();
    }

    pub(super) fn handle_send_selection_to_ai(
        &mut self,
        _action: &SendSelectionToAi,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_ai_panel_context(cx);
        let Some(sel) = self
            .tabs
            .get(self.active_tab)
            .and_then(|tab| tab.editor.read(cx).selected_text())
            .filter(|text| !text.trim().is_empty())
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.select_sql_first"),
            );
            cx.notify();
            return;
        };
        const MAX_SQL: usize = 4_000;
        let mut bounded: String = sel.chars().take(MAX_SQL).collect();
        if sel.chars().count() > MAX_SQL {
            bounded.push_str(&crate::localization::text("shell.ai.truncated_sql_comment"));
        }
        let root = self.project_file_picker_root.clone();
        self.ai_panel.update(cx, |panel, cx| {
            panel.set_editor_selection(Some(sel.clone()));
            panel.set_project_root(root);
            panel.open_ask_with_prompt(
                crate::localization::format(
                    "shell.ai.explain_selection_prompt",
                    &[("sql", &bounded)],
                ),
                cx,
            );
        });
        self.ai_visible = true;
        cx.notify();
    }

    /// Open the AI drawer seeded with a plain-language "explain this error"
    /// prompt for the most recent failed query. No-op with no recorded error.
    pub(super) fn handle_explain_last_error(&mut self, cx: &mut Context<Self>) {
        let Some(failure) = self.last_query_error.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.no_recent_error"),
            );
            cx.notify();
            return;
        };
        // The SQL comes from the recorded failure, not the active editor: the
        // user may have switched tabs or edited since, and the prompt must not
        // assert that unrelated SQL produced this error. The fix context is
        // captured NOW, bound to the failing tab, and travels with the
        // conversation; with the tab gone there is nothing to replace into,
        // so the conversation is explain-only.
        let context = self.error_fix_context_for(&failure, cx);
        self.ai_panel.update(cx, |p, cx| {
            p.open_explain_error(&failure.error, &failure.sql, context, cx)
        });
        self.ai_visible = true;
        cx.notify();
    }

    /// Bind a recorded failure to its tab: the run region, the tab's run
    /// generation, and the failing statement's span, as of right now.
    fn error_fix_context_for(
        &self,
        failure: &LastQueryError,
        cx: &mut Context<Self>,
    ) -> Option<ErrorFixContext> {
        let index = self.tab_index_by_id(&failure.tab_id)?;
        let tab = self.tabs.get(index)?;
        if tab.snapshot_only || tab.ddl_view.is_some() || tab.notebook.is_some() {
            return None;
        }
        let db_type = tab.editor.read(cx).database_type();
        let statement_span =
            failing_statement_span(&failure.sql, &failure.error, db_type, tab_auto_wrap(tab));
        Some(ErrorFixContext {
            tab_id: failure.tab_id.clone(),
            error: failure.error.clone(),
            run_sql: failure.sql.clone(),
            last_executed_span: tab.last_executed_span,
            run_generation: tab.results.read(cx).run_generation(),
            statement_span,
        })
    }

    /// "Replace failing statement" on an AI error-fix answer: swap the failing
    /// statement in its originating tab for the AI's fixed SQL as ONE
    /// undoable edit, then select the new text so the change is visible.
    /// Works only from the context captured when the conversation started,
    /// and only in that tab. When the statement cannot be located honestly
    /// (the text drifted, or the engine reported no position) the fix is
    /// inserted at that tab's caret with the reason stated; when the tab is
    /// gone or has run again since, nothing is changed and the fix is
    /// disarmed.
    fn replace_failing_statement(
        &mut self,
        fixed_sql: String,
        context: ErrorFixContext,
        cx: &mut Context<Self>,
    ) {
        let fixed = fixed_sql_for_replacement(&fixed_sql);
        let disarm = |this: &mut Self, level: StatusLevel, note: &str, cx: &mut Context<Self>| {
            this.ai_panel.update(cx, |p, cx| p.disarm_error_fix(cx));
            this.set_status(level, note.to_string());
            cx.notify();
        };
        let Some(index) = self.tab_index_by_id(&context.tab_id) else {
            disarm(
                self,
                StatusLevel::Info,
                crate::localization::text("shell.ai.fix_tab_closed").as_str(),
                cx,
            );
            return;
        };
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.snapshot_only || tab.ddl_view.is_some() || tab.notebook.is_some() {
            disarm(
                self,
                StatusLevel::Info,
                crate::localization::text("shell.ai.fix_not_editable").as_str(),
                cx,
            );
            return;
        }
        if tab.results.read(cx).run_generation() != context.run_generation {
            disarm(
                self,
                StatusLevel::Info,
                crate::localization::text("shell.ai.fix_stale_run").as_str(),
                cx,
            );
            return;
        }
        let editor = tab.editor.clone();
        if index != self.active_tab {
            self.activate_tab_for_ai_fix(index, cx);
        }
        if fixed.is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.fix_empty_sql"),
            );
            cx.notify();
            return;
        }
        let (buffer, db_type) = {
            let e = editor.read(cx);
            (e.text().to_string(), e.database_type())
        };
        let Some((start, end, whole_run)) =
            ai_fix_replacement_plan(&buffer, &context, &fixed, db_type)
        else {
            editor.update(cx, |e, cx| e.insert(&fixed, cx));
            let key = if context.statement_span.is_none() {
                "shell.ai.fix_missing_position"
            } else {
                "shell.ai.fix_changed_statement"
            };
            self.set_status(StatusLevel::Info, crate::localization::text(key));
            cx.notify();
            return;
        };
        // `replace_document` makes the swap ONE undoable edit (Ctrl+Z brings
        // the old statement back) and emits no Change event, so drive what a
        // typed edit would: dirty flag, proc-mode rescan, session autosave.
        let mut new_text = String::with_capacity(buffer.len() - (end - start) + fixed.len());
        new_text.push_str(&buffer[..start]);
        new_text.push_str(&fixed);
        new_text.push_str(&buffer[end..]);
        let proc_dialect = sqlly_services::proc_dialect(db_type);
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.proc_mode
                .refresh_from_script_for(&new_text, proc_dialect);
            tab.dirty = true;
        }
        // `fixed` is already `\n`-normalized, like the stored document, so
        // this offset lands exactly on the end of the inserted text.
        let select_end = start + fixed.len();
        editor.update(cx, |e, cx| {
            e.replace_document(new_text, cx);
            e.adopt_caret_state(select_end, Some(start), cx);
        });
        self.schedule_session_autosave(cx);
        // Applied: the context's offsets no longer describe the buffer.
        self.ai_panel.update(cx, |p, cx| p.disarm_error_fix(cx));
        self.set_status(
            StatusLevel::Success,
            if whole_run {
                crate::localization::text("shell.ai.fix_replaced_whole_run")
            } else {
                crate::localization::text("shell.ai.fix_replaced_statement")
            },
        );
        cx.notify();
    }

    fn activate_tab_for_ai_fix(&mut self, index: usize, cx: &mut Context<Self>) {
        self.active_tab = index;
        self.record_tab_activation(index);
        self.retarget_same_buffer_view(cx);
        self.sync_toolbar_to_active_tab();
    }

    /// Open the AI drawer seeded with a digest of the captured execution plan
    /// (findings + expensive operators) and a request to explain it. No-op when
    /// no plan has been captured for the active tab.
    pub(super) fn handle_explain_plan_with_ai(&mut self, cx: &mut Context<Self>) {
        let prompt = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.results.read(cx).ai_plan_explain_prompt(cx));
        let Some(prompt) = prompt else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.capture_plan_first"),
            );
            cx.notify();
            return;
        };
        self.ai_panel
            .update(cx, |p, cx| p.open_ask_with_prompt(prompt, cx));
        self.ai_visible = true;
        cx.notify();
    }

    /// Open the AI drawer seeded with context from the active tab's most
    /// recent query run, including live error text and plan digest when present.
    pub(super) fn handle_analyze_last_run_with_ai(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.no_active_query"),
            );
            cx.notify();
            return;
        };
        let Some(sql) = tab.last_run_sql.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.run_query_first"),
            );
            cx.notify();
            return;
        };
        let results = tab.results.read(cx);
        let error = results.execution_error().map(str::to_string);
        let outcome = if error.is_some() {
            "failed"
        } else if results.is_running() {
            "unknown"
        } else {
            "succeeded"
        };
        let profile = self.active_profile.as_ref();
        let server = profile
            .map(|p| p.name.as_str())
            .or_else(|| profile.map(|p| p.server.as_str()))
            .unwrap_or("unknown");
        let database = profile.and_then(|p| p.database.as_deref());
        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let plan_digest = results.ai_plan_explain_prompt(cx);
        let input = crate::ai_history_prompt::HistoryAnalysisInput {
            sql: &sql,
            server,
            database,
            timestamp: &timestamp,
            duration_ms: results.elapsed_ms(),
            outcome,
            rows_affected: Some(results.total_rows() as u64),
            error_text: error.as_deref(),
            plan_digest: plan_digest.as_deref(),
        };
        let prompt = crate::ai_history_prompt::build_history_analysis_prompt(&input);
        self.ai_panel
            .update(cx, |p, cx| p.open_ask_with_prompt(prompt, cx));
        self.ai_visible = true;
        cx.notify();
    }

    /// Precompute the AI cache and vector embeddings for the active connection's
    /// catalog, then report the embedding counts. Long-running and cancellable
    /// by disconnecting; needs a connected profile.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_build_ai_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.connect_build_catalog"),
            );
            cx.notify();
            return;
        };
        let services = self.services_for_profile(&profile);
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.ai.building_catalog"),
        );
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = match services.catalog.build_ai_cache(&profile).await {
                        Ok(()) => services
                            .catalog
                            .build_embeddings(&profile)
                            .await
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let mut async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok(result) = rx.await {
                weak_self
                    .update(&mut async_cx, |this, cx| {
                        match result {
                            Ok(report) => this.set_status(
                                StatusLevel::Success,
                                crate::localization::format(
                                    "shell.ai.catalog_built",
                                    &[
                                        ("embedded", &report.embedded.to_string()),
                                        ("processed", &report.processed.to_string()),
                                        ("failed", &report.failed.to_string()),
                                        ("stale", &report.remaining_stale.to_string()),
                                    ],
                                ),
                            ),
                            Err(error) => this.set_status(
                                StatusLevel::Error,
                                crate::localization::format(
                                    "shell.ai.catalog_build_failed",
                                    &[("error", &error)],
                                ),
                            ),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Wipe all AI-derived catalog data (cards + embeddings) for the active
    /// connection, keeping structural catalog knowledge. Needs a connected
    /// profile.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_invalidate_ai_data(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ai.connect_clear_data"),
            );
            cx.notify();
            return;
        };
        let services = self.services_for_profile(&profile);
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.ai.clearing_catalog"),
        );
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = services
                        .catalog
                        .invalidate_ai_cache(&profile)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let mut async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok(result) = rx.await {
                weak_self
                    .update(&mut async_cx, |this, cx| {
                        match result {
                            Ok(report) => this.set_status(
                                StatusLevel::Success,
                                crate::localization::format(
                                    "shell.ai.data_cleared",
                                    &[
                                        ("cards", &report.invalidated.to_string()),
                                        ("embeddings", &report.embeddings_deleted.to_string()),
                                    ],
                                ),
                            ),
                            Err(error) => this.set_status(
                                StatusLevel::Error,
                                crate::localization::format(
                                    "shell.ai.clear_failed",
                                    &[("error", &error)],
                                ),
                            ),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TDS: sqlly_core::DatabaseType = sqlly_core::DatabaseType::SqlServer;
    const PG: sqlly_core::DatabaseType = sqlly_core::DatabaseType::Postgres;
    const MYSQL: sqlly_core::DatabaseType = sqlly_core::DatabaseType::MySql;

    fn range_of(haystack: &str, needle: &str) -> (usize, usize) {
        let start = haystack.find(needle).unwrap();
        (start, start + needle.len())
    }

    #[test]
    fn fixed_sql_trims_and_drops_one_terminator() {
        assert_eq!(fixed_sql_for_replacement("  SELECT 2;  "), "SELECT 2");
        assert_eq!(fixed_sql_for_replacement("SELECT 2"), "SELECT 2");
        assert_eq!(
            fixed_sql_for_replacement("UPDATE t SET x = 1;\nSELECT 2;\n"),
            "UPDATE t SET x = 1;\nSELECT 2"
        );
        assert_eq!(fixed_sql_for_replacement("   "), "");
    }

    #[test]
    fn region_prefers_the_recorded_run_span() {
        let buffer = "SELECT 1;\nSELECT 1;";
        // Two identical copies: without the recorded span this is drift, with
        // it the second copy is found precisely.
        assert_eq!(ai_fix_region_start(buffer, "SELECT 1;", None), None);
        assert_eq!(
            ai_fix_region_start(buffer, "SELECT 1;", Some((10, 19))),
            Some(10)
        );
    }

    #[test]
    fn region_detects_edits_inside_and_above_the_run() {
        let run = "SELECT id FORM users;";
        let buffer = format!("-- header\n{run}\n");
        // An edit above the run shifts offsets but the text is still findable.
        assert_eq!(
            ai_fix_region_start(&buffer, run, Some((0, run.len()))),
            Some("-- header\n".len()),
            "stale span falls through to unique containment"
        );
        // An edit inside the statement hides the run text entirely.
        let edited = buffer.replace("FORM", "FROM");
        assert_eq!(ai_fix_region_start(&edited, run, None), None);
        assert!(ai_fix_region_start("", run, None).is_none());
        assert!(ai_fix_region_start(&buffer, "", None).is_none());
    }

    #[test]
    fn tds_error_line_expands_to_the_failing_statement() {
        let run_sql = "SELECT 1;\nSELECT Nmae FROM users;\nSELECT 3;";
        let error = "Msg 207, Level 16, State 1, Line 2\nInvalid column name 'Nmae'.";
        let (start, end) = failing_statement_span(run_sql, error, TDS, false).unwrap();
        // The slice excludes the terminator: replacing it leaves the original
        // `;` in place.
        assert_eq!(&run_sql[start..end], "SELECT Nmae FROM users");
        assert_eq!(&run_sql[end..end + 1], ";");
    }

    #[test]
    fn tds_span_respects_go_batches() {
        let run_sql = "SELECT 1\nGO\nSELECT Nmae FROM t\nGO\nSELECT 3";
        let error = "Msg 207, Level 16, State 1, Line 3\nInvalid column name 'Nmae'.";
        let (start, end) = failing_statement_span(run_sql, error, TDS, false).unwrap();
        assert_eq!(&run_sql[start..end], "SELECT Nmae FROM t");
    }

    #[test]
    fn pg_position_maps_to_statement() {
        let run_sql = "SELECT 1;\nSELECT * FROM nowhere_table;\nSELECT 3;";
        let at = run_sql.find("nowhere_table").unwrap() + 1; // POSITION is 1-based
        let error = format!("relation \"nowhere_table\" does not exist\nPOSITION: {at}");
        let (start, end) = failing_statement_span(run_sql, &error, PG, false).unwrap();
        assert_eq!(&run_sql[start..end], "SELECT * FROM nowhere_table");
    }

    #[test]
    fn mysql_line_maps_to_statement() {
        let run_sql = "SELECT 1;\nSELECT a FORM t;\nSELECT 3;";
        let error =
            "ERROR 1064 (42000): You have an error in your SQL syntax near 'FORM t' at line 2";
        let (start, end) = failing_statement_span(run_sql, error, MYSQL, false).unwrap();
        assert_eq!(&run_sql[start..end], "SELECT a FORM t");
    }

    #[test]
    fn comment_above_the_statement_survives_the_span() {
        let run_sql = "SELECT 1;\n-- daily revenue\nSELECT revenu FROM sales;\nSELECT 3;";
        let error = "Msg 207, Level 16, State 1, Line 3\nInvalid column name 'revenu'.";
        let (start, end) = failing_statement_span(run_sql, error, TDS, false).unwrap();
        assert_eq!(&run_sql[start..end], "SELECT revenu FROM sales");
        let replaced = format!(
            "{}SELECT revenue FROM sales{}",
            &run_sql[..start],
            &run_sql[end..]
        );
        assert!(replaced.contains("-- daily revenue\nSELECT revenue FROM sales;"));
    }

    fn context(run_sql: &str, error: &str, db: sqlly_core::DatabaseType) -> ErrorFixContext {
        ErrorFixContext {
            tab_id: "tab-1".into(),
            error: error.into(),
            run_sql: run_sql.into(),
            last_executed_span: None,
            run_generation: 3,
            statement_span: failing_statement_span(run_sql, error, db, false),
        }
    }

    #[test]
    fn full_replacement_range_composes_region_and_statement() {
        let run_sql = "SELECT 1;\nSELECT Nmae FROM users;";
        let buffer = format!("-- leading comment\n\n{run_sql}");
        let error = "Msg 207, Level 16, State 1, Line 2\nInvalid column name 'Nmae'.";
        let ctx = context(run_sql, error, TDS);
        assert_eq!(ctx.failing_statement(), Some("SELECT Nmae FROM users"));
        let (start, end, whole) =
            ai_fix_replacement_plan(&buffer, &ctx, "SELECT Name FROM users", TDS).unwrap();
        assert!(!whole);
        assert_eq!(&buffer[start..end], "SELECT Nmae FROM users");
        let (want_start, want_end) = range_of(&buffer, "SELECT Nmae FROM users");
        assert_eq!((start, end), (want_start, want_end));
    }

    /// Regression: an answer that repeats the whole script used to be pasted
    /// over the one failing statement, duplicating everything around it.
    #[test]
    fn whole_script_answers_replace_the_whole_run() {
        let run_sql = "SELECT 1;\nSELECT Nmae FROM users;\nSELECT 3;";
        let buffer = format!("-- top\n{run_sql}\n-- after");
        let error = "Msg 207, Level 16, State 1, Line 2\nInvalid column name 'Nmae'.";
        let ctx = context(run_sql, error, TDS);
        let answer = fixed_sql_for_replacement("SELECT 1;\nSELECT Name FROM users;\nSELECT 3;");
        let (start, end, whole) = ai_fix_replacement_plan(&buffer, &ctx, &answer, TDS).unwrap();
        assert!(whole, "the answer repeats the other statements");
        let replaced = format!("{}{}{}", &buffer[..start], answer, &buffer[end..]);
        assert_eq!(
            replaced,
            "-- top\nSELECT 1;\nSELECT Name FROM users;\nSELECT 3;\n-- after"
        );
        assert_eq!(
            replaced.matches("SELECT 3").count(),
            1,
            "nothing duplicated"
        );

        // A two-statement fix that does not repeat the script stays scoped to
        // the failing statement.
        let scoped = "SET NOCOUNT ON;\nSELECT Name FROM users";
        let (start, end, whole) = ai_fix_replacement_plan(&buffer, &ctx, scoped, TDS).unwrap();
        assert!(!whole);
        assert_eq!(&buffer[start..end], "SELECT Nmae FROM users");
    }

    #[test]
    fn whole_run_detection_ignores_whitespace_and_terminators() {
        let run_sql = "SELECT 1;\nSELECT Nmae FROM users;";
        let span = range_of(run_sql, "SELECT Nmae FROM users");
        let dialect = TDS.family();
        assert!(fix_rewrites_whole_run(
            run_sql,
            span,
            "SELECT   1\n;\nSELECT Name\n  FROM users",
            dialect
        ));
        assert!(!fix_rewrites_whole_run(
            run_sql,
            span,
            "SELECT Name FROM users",
            dialect
        ));
        // A single-statement script can never be "rewritten whole" beyond
        // the statement itself.
        assert!(!fix_rewrites_whole_run(
            "SELECT Nmae FROM users",
            (0, 22),
            "SELECT 1; SELECT Name FROM users",
            dialect
        ));
    }

    #[test]
    fn crlf_answers_are_normalized_before_offsets_are_taken() {
        let fixed = fixed_sql_for_replacement("SELECT a,\r\n       b\r\nFROM t;\r\n");
        assert_eq!(fixed, "SELECT a,\n       b\nFROM t");
        assert!(!fixed.contains('\r'));
    }

    #[test]
    fn a_context_without_a_located_statement_has_no_plan() {
        let ctx = context("GET x", "unknown command", TDS);
        assert_eq!(ctx.failing_statement(), None);
        assert!(ai_fix_replacement_plan("GET x", &ctx, "GET y", TDS).is_none());
    }

    #[test]
    fn unpositioned_errors_yield_no_span() {
        // Redis/ClickHouse-style messages carry no line or position: no span,
        // so the caller falls back instead of guessing.
        assert!(failing_statement_span("GET x", "unknown command", TDS, false).is_none());
        assert!(
            failing_statement_span("", "Msg 207, Level 16, State 1, Line 1\nx", TDS, false)
                .is_none()
        );
    }
}
