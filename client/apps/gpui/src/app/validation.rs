use super::*;

fn map_validation_result(
    result: &sqlly_ai::catalog::SqlValidationResult,
    sql: &str,
) -> Vec<crate::editor::ValidationDiagnostic> {
    use crate::editor::DiagnosticSeverity;
    use sqlly_ai::catalog::SqlIssueSeverity;

    result
        .issues
        .iter()
        .map(|issue| {
            let severity = match issue.severity {
                SqlIssueSeverity::Info => DiagnosticSeverity::Info,
                SqlIssueSeverity::Warning => DiagnosticSeverity::Warning,
                SqlIssueSeverity::Error => DiagnosticSeverity::Error,
                SqlIssueSeverity::RequireConfirmation => DiagnosticSeverity::RequireConfirmation,
            };
            let (start_byte, end_byte, line) = if let Some(span) = issue.span {
                let line = sql[..span.start.min(sql.len())].matches('\n').count();
                (span.start, span.end, line)
            } else {
                let line = issue
                    .identifier
                    .as_ref()
                    .and_then(|id| find_line_for_identifier(sql, id))
                    .unwrap_or(0);
                (0, 0, line)
            };
            crate::editor::ValidationDiagnostic {
                severity,
                source: crate::editor::DiagnosticSource::Validation,
                message: issue.message.clone(),
                line,
                identifier: issue.identifier.clone(),
                start_byte,
                end_byte,
                explainer: None,
                fix: None,
            }
        })
        .collect()
}

fn find_line_for_identifier(sql: &str, identifier: &str) -> Option<usize> {
    let id_lower = identifier.to_lowercase();
    for (line_idx, line) in sql.lines().enumerate() {
        if line.to_lowercase().contains(&id_lower) {
            return Some(line_idx);
        }
    }
    None
}

/// Best-effort extraction of the most recent table reference (`FROM <t>` or
/// `JOIN <t>`) from a SQL string, preserving schema qualification. Returns the
/// raw token (e.g. `dbo.Customers`).
fn extract_table_reference(sql: &str) -> Option<String> {
    let lower = sql.to_lowercase();
    let mut last: Option<String> = None;
    for keyword in ["from ", "join "] {
        let bytes = lower.as_bytes();
        let mut start = 0usize;
        while let Some(idx) = lower[start..].find(keyword) {
            let abs = start + idx;
            let after = abs + keyword.len();
            let rest = &sql[after..];
            let token: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                .collect();
            if !token.is_empty() {
                last = Some(token);
            }
            start = after;
            if start >= bytes.len() {
                break;
            }
        }
    }
    last
}

/// Collects the first column of every row across all `Rows` events until the
/// stream terminates, formatting each value as a display string.
async fn collect_first_column(handle: sqlly_services::QueryHandle) -> Option<Vec<String>> {
    use sqlly_core::CellValue;
    fn push_first_cells(rows: Vec<Vec<CellValue>>, out: &mut Vec<String>) {
        for row in rows {
            if let Some(cell) = row.first() {
                let s = match cell {
                    CellValue::Null => continue,
                    CellValue::Bool(b) => b.to_string(),
                    CellValue::I64(n) => n.to_string(),
                    CellValue::F64(n) => n.to_string(),
                    CellValue::String(s) => s.clone(),
                    CellValue::Bytes(b) => format!("{:?}", b),
                };
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    let mut events = handle.events;
    let mut out: Vec<String> = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            sqlly_services::QueryEvent::Rows { rows, .. } => {
                push_first_cells(rows, &mut out);
            }
            sqlly_services::QueryEvent::SpillManifest { manifest } => {
                // Spilled sets hold the COMPLETE rows; the `contains` dedup
                // makes re-reading the already-forwarded prefix harmless.
                for set in &manifest.result_sets {
                    if let Some(rows) = sqlly_services::take_spilled_rows(set) {
                        push_first_cells(rows, &mut out);
                    }
                }
            }
            sqlly_services::QueryEvent::Error { .. } => return Some(out),
            sqlly_services::QueryEvent::Completed { .. }
            | sqlly_services::QueryEvent::Cancelled => {
                return Some(out);
            }
            _ => {}
        }
    }
    Some(out)
}

/// Row cap on a value-lookup fetch. A fetch that returns this many rows is
/// only a sample, so the editor re-queries with the typed text once it stops
/// matching the sample (feedback ifb_2fc6).
const VALUE_LOOKUP_FETCH_LIMIT: usize = 100;

impl AppView {
    /// Host the "Improve IntelliSense" dialog above the workbench for a
    /// context the editor just captured. Root-hosted (not editor-rendered) so
    /// the modal centers over the whole window instead of clipping to the
    /// query editor pane; its title bar carries a pop-out into a standalone
    /// OS window.
    pub(super) fn open_intellisense_feedback_dialog(
        &mut self,
        snapshot: crate::intellisense_feedback::FeedbackSnapshot,
        include_all_default: bool,
        kind_defaults: crate::feedback_tree::KindVisibility,
        font: gpui::Font,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|_| {
            crate::intellisense_feedback_dialog::IntelliSenseFeedbackDialog::new(
                snapshot,
                include_all_default,
                kind_defaults,
                font,
            )
        });
        cx.subscribe(&dialog, Self::handle_feedback_dialog_event)
            .detach();
        self.overlays.intellisense_feedback = Some(dialog);
        cx.notify();
    }

    pub(super) fn handle_feedback_dialog_event(
        &mut self,
        _dialog: Entity<crate::intellisense_feedback_dialog::IntelliSenseFeedbackDialog>,
        event: &crate::intellisense_feedback_dialog::FeedbackDialogEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::intellisense_feedback_dialog::FeedbackDialogEvent;
        match event {
            FeedbackDialogEvent::Submitted {
                expected_text,
                snapshot,
            } => {
                self.overlays.intellisense_feedback = None;
                self.restore_workbench_focus(None, cx);
                #[cfg(not(target_arch = "wasm32"))]
                self.queue_intellisense_feedback(expected_text.clone(), (**snapshot).clone(), cx);
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = (expected_text, snapshot);
                    self.set_status(
                        StatusLevel::Error,
                        crate::store::StoreError::BrowserUnsupported.to_string(),
                    );
                }
                cx.notify();
            }
            FeedbackDialogEvent::Dismissed => {
                self.overlays.intellisense_feedback = None;
                self.restore_workbench_focus(None, cx);
                cx.notify();
            }
            FeedbackDialogEvent::PoppedOut => {
                // The standalone window owns the entity now; the detached
                // subscription above keeps routing its submission.
                self.overlays.intellisense_feedback = None;
                cx.notify();
            }
        }
    }

    /// A submitted "Improve IntelliSense" dialog (the dialog itself closed
    /// immediately): serialize + compress the disclosure-filtered context off
    /// the UI thread, write the report to its pending file stamped with a
    /// fresh GUID, then try the upload at once. A failed upload leaves the
    /// file for the background worker (`feedback_uploader::sync`), which
    /// retries every five minutes until the server accepts it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn queue_intellisense_feedback(
        &mut self,
        expected_text: String,
        snapshot: crate::intellisense_feedback::FeedbackSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.set_status(
            StatusLevel::Busy,
            crate::localization::text("shell.validation.sending_feedback"),
        );
        cx.notify();

        let device_id = crate::store::pairing::load().map(|p| p.device_id);
        let weak_self = cx.weak_entity();
        let mut async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            // Serialize + compress off the UI thread — a large opted-in
            // catalog makes this JSON big.
            let outcome = crate::tokio_handle()
                .spawn(async move {
                    let context_brotli = crate::intellisense_feedback::brotli_compress(
                        &snapshot.to_json_bytes_within(
                            crate::intellisense_feedback::CONTEXT_BUDGET_BYTES,
                        ),
                    );
                    let record = crate::feedback_uploader::PendingFeedback {
                        submission_id: uuid::Uuid::new_v4().to_string(),
                        created_at: chrono::Utc::now(),
                        device_id,
                        expected_text,
                        context_brotli_b64: base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &context_brotli,
                        ),
                        attempts: 0,
                        next_attempt_at: None,
                    };
                    crate::feedback_uploader::save(&record);
                    crate::feedback_uploader::sync::upload_intellisense(record).await
                })
                .await
                .map_err(|e| e.to_string());
            let _ = weak_self.update(&mut async_cx, |this, cx| {
                let (level, text) =
                    super::general_feedback::feedback_status(outcome, "IntelliSense feedback");
                this.set_status(level, text);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn handle_toggle_intellisense_debug(
        &mut self,
        _action: &ToggleIntelliSenseDebug,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lsp_debug.update(cx, |v, cx| v.toggle_visibility(cx));
        if self.lsp_debug.read(cx).is_visible() {
            self.refresh_lsp_debug(cx);
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.validation.debug_refreshing"),
            );
        } else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.validation.debug_hidden"),
            );
        }
        cx.notify();
    }

    /// Rebuild the LSP debug snapshot from the active tab + app state, refresh
    /// the sync sections of the debug panel, and spawn the async diagnostics
    /// (`validate_sql`) on the tokio runtime — the exact same `Services` calls
    /// the `sqlly-lsp` server makes, rendered in LSP wire format. The spawned
    /// diagnostics task carries the refresh generation so a stale result from a
    /// prior refresh is discarded by `set_diagnostics`.
    pub(super) fn refresh_lsp_debug(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            self.lsp_debug.update(cx, |v, cx| v.clear(cx));
            return;
        };
        let editor = tab.editor.clone();
        let text = editor.read(cx).content().to_string();
        let cursor = editor.read(cx).cursor();
        let snapshot = crate::lsp_debug::LspDebugSnapshot {
            text: text.clone(),
            cursor,
            profile: self.active_profile.clone(),
            catalog: (*self.completion_catalog).clone(),
            procedures: (*self.completion_procedures).clone(),
            rules: self.completion_rules.clone(),
        };
        // Same routing as the features the panel debugs: the diagnostics run
        // against the active profile's route (relay-backed for Via-Relay).
        let services = self
            .active_profile
            .as_ref()
            .map(|p| self.services_for_profile(p))
            .unwrap_or_else(|| self.services.clone());
        let refresh_gen = self
            .lsp_debug
            .update(cx, |v, cx| v.refresh(&snapshot, &services, cx));

        // Spawn the async diagnostics (validate_sql) on the tokio runtime, then
        // hand the serialized result back to the view. Mirrors `validate_sql`.
        // The result is wrapped in `DocumentDiagnosticReport` — the exact wire
        // shape the LSP pull-handler (`spawn_diagnostic`) returns.
        let profile = self.active_profile.clone();
        let diag_services = services;
        let sql = text.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let diags = sqlly_lsp::compute_diagnostics(
                        &diag_services,
                        profile.as_ref(),
                        Some(&sql),
                    )
                    .await;
                    let report = sqlly_lsp::DocumentDiagnosticReport {
                        kind: "full".to_string(),
                        items: diags,
                    };
                    let json = serde_json::to_string_pretty(&report)
                        .unwrap_or_else(|e| format!("serialization error: {e}"));
                    let _ = tx.send(json).await;
                })
                .await
                .ok();
        })
        .detach();

        let weak_view = self.lsp_debug.downgrade();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Some(json) = rx.recv().await {
                let mut cx = async_cx.clone();
                weak_view
                    .update(&mut cx, |v, cx| v.set_diagnostics(json, refresh_gen, cx))
                    .ok();
            }
        })
        .detach();
    }

    pub(super) fn handle_lsp_debug_event(
        &mut self,
        _view: Entity<crate::lsp_debug::LspDebugView>,
        event: &crate::lsp_debug::LspDebugEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::lsp_debug::LspDebugEvent::RefreshRequested => {
                self.refresh_lsp_debug(cx);
            }
            crate::lsp_debug::LspDebugEvent::VisibilityChanged(visible) => {
                self.set_status(
                    StatusLevel::Info,
                    if *visible {
                        crate::localization::text("shell.validation.debug_shown")
                    } else {
                        crate::localization::text("shell.validation.debug_visibility_hidden")
                    },
                );
                cx.notify();
            }
        }
    }

    pub(super) fn validate_sql(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        self.validate_sql_impl(tab_idx, false, cx);
    }

    pub(super) fn validate_sql_silent(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        self.validate_sql_impl(tab_idx, true, cx);
    }

    pub(super) fn validate_sql_impl(
        &mut self,
        tab_idx: usize,
        silent: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            if !silent {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.validation.connect_first"),
                );
                cx.notify();
            }
            return;
        };

        let (editor_entity, results_entity) = {
            let tab = match self.tabs.get(tab_idx) {
                Some(t) => t,
                None => return,
            };
            (tab.editor.clone(), tab.results.clone())
        };

        let sql = editor_entity.read(cx).content().to_string();
        if sql.trim().is_empty() {
            if !silent {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.validation.nothing_to_validate"),
                );
                cx.notify();
            }
            editor_entity.update(cx, |e, cx| e.clear_diagnostics(cx));
            results_entity.update(cx, |r, cx| r.set_diagnostics(Vec::new(), cx));
            return;
        }
        // Redis: no SQL parser applies. Check the command lines instead —
        // unterminated quotes, bad JSON lines, and commands that would block
        // the connection forever — and show those as diagnostics.
        if let Some(reason) = crate::capabilities::sql_editing(profile.database_type).reason() {
            let diagnostics = crate::mutation_review::redis_command_diagnostics(&sql);
            let count = diagnostics.len();
            editor_entity.update(cx, |e, cx| e.set_diagnostics(diagnostics.clone(), cx));
            results_entity.update(cx, |r, cx| r.set_diagnostics(diagnostics, cx));
            if !silent {
                self.set_status(
                    StatusLevel::Info,
                    if count == 0 {
                        crate::localization::format(
                            "shell.validation.commands_valid",
                            &[("reason", reason)],
                        )
                    } else {
                        crate::localization::format(
                            if count == 1 {
                                "shell.validation.command_problem_singular"
                            } else {
                                "shell.validation.command_problem_plural"
                            },
                            &[("count", &count.to_string())],
                        )
                    },
                );
                cx.notify();
            }
            return;
        }
        self.log.log_verbose(
            "validation",
            &crate::localization::format(
                "shell.validation.log_validating",
                &[("count", &sql.len().to_string())],
            ),
        );
        // Validation is engine-backed (`catalog.validate_sql`), so it follows
        // the profile's route like execution does.
        let services = self.services_for_profile(&profile);
        // Validate in the dialect the profile actually speaks — the same rule
        // the LSP diagnostics path already follows.
        let dialect = profile.database_type.family();
        let sql_for_map = sql.clone();
        if !silent {
            self.set_status(
                StatusLevel::Busy,
                crate::localization::text("shell.validation.validating"),
            );
            cx.notify();
        }

        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<sqlly_ai::catalog::SqlValidationResult, String>>(1);

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = services.catalog.validate_sql(&profile, &sql, dialect).await;
                    let _ = tx.send(result.map_err(|e| e.to_string())).await;
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Some(result) = rx.recv().await {
                let sql = sql_for_map;
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        match result {
                            Ok(validation) => {
                                let mut diagnostics = map_validation_result(&validation, &sql);
                                // Client-side directive lint rides along with
                                // every validation pass: a typo'd `-- sqlly`
                                // comment squiggles instead of silently doing
                                // nothing.
                                diagnostics.extend(
                                    crate::sqlly_directive::directive_diagnostics(&sql)
                                        .into_iter()
                                        .map(|d| crate::editor::ValidationDiagnostic {
                                            severity: crate::editor::DiagnosticSeverity::Warning,
                                            source: crate::editor::DiagnosticSource::Validation,
                                            line: sql[..d.start.min(sql.len())]
                                                .matches('\n')
                                                .count(),
                                            message: d.message,
                                            identifier: None,
                                            start_byte: d.start,
                                            end_byte: d.end,
                                            explainer: None,
                                            fix: None,
                                        }),
                                );
                                let _count = diagnostics.len();
                                editor_entity.update(cx, |e, cx| {
                                    e.set_diagnostics(diagnostics.clone(), cx);
                                });
                                // The editor already underlines GROUP BY
                                // findings live (a Lint entry it keeps across
                                // validations); list them in the findings
                                // panel too so Validate SQL shows everything.
                                let mut findings = diagnostics;
                                findings.extend(
                                    crate::editor::group_by_lint::group_by_lint_diagnostics(
                                        &sql, dialect,
                                    ),
                                );
                                results_entity.update(cx, |r, cx| {
                                    r.set_diagnostics(findings, cx);
                                });
                                if !silent {
                                    let summary =
                                        crate::sql_validation_formatting::summary(&validation);
                                    this.set_status(
                                        StatusLevel::Info,
                                        crate::localization::format(
                                            "shell.validation.summary",
                                            &[("summary", &summary)],
                                        ),
                                    );
                                }
                            }
                            Err(e) => {
                                editor_entity.update(cx, |e, cx| e.clear_diagnostics(cx));
                                if !silent {
                                    this.set_status(
                                        StatusLevel::Error,
                                        crate::localization::format(
                                            "shell.validation.error",
                                            &[("error", &e)],
                                        ),
                                    );
                                }
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Runs a distinct-values lookup for `word` against the most-recent table
    /// reference in `sql`, then inserts the values into the tab's editor as a
    /// comma-separated list (handy for building `IN (...)` clauses). Requires an
    /// active connection; otherwise surfaces a status message.
    pub(super) fn run_value_lookup(
        &mut self,
        tab_idx: usize,
        word: String,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let word = word.trim().to_string();
        if word.is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.validation.cursor_column_required"),
            );
            cx.notify();
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.validation.connect_lookup_values"),
            );
            cx.notify();
            return;
        };
        let Some(table) = extract_table_reference(&sql) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.validation.table_reference_unresolved"),
            );
            cx.notify();
            return;
        };

        let query = distinct_word_lookup_sql(profile.database_type, &word, &table, 20);
        let list_db = profile.database_type;
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.validation.lookup_distinct_progress",
                &[("column", &word)],
            ),
        );
        cx.notify();

        let (editor_entity, _results_entity) = {
            let tab = match self.tabs.get(tab_idx) {
                Some(t) => t,
                None => return,
            };
            (tab.editor.clone(), tab.results.clone())
        };
        // The DISTINCT lookup runs on the server, so it follows the profile's
        // route like any other query.
        let services = self.services_for_profile(&profile);
        let lookup_word = word.clone();
        // A failed lookup comes back as raw error text and is worded on the
        // UI thread below (translation state is per thread).
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Option<Vec<String>>, String>>();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = match services.queries.execute(&profile, &query, 64).await {
                        Ok(handle) => Ok(collect_first_column(handle).await),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let vals = match rx.await {
                Ok(Ok(Some(vals))) => vals,
                Ok(Err(error)) => vec![crate::localization::format(
                    "shell.validation.lookup_error",
                    &[("error", &error)],
                )],
                _ => return,
            };
            {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        // Fresh engine literals for every value: stored text
                        // (even one starting with a quote) never becomes SQL.
                        let snippet =
                            sqlly_services::intellisense::value_lookup::distinct_values_list(
                                list_db, &vals,
                            );
                        editor_entity.update(cx, |e, cx| e.insert(&snippet, cx));
                        this.set_status(
                            StatusLevel::Success,
                            crate::localization::format(
                                if vals.len() == 1 {
                                    "shell.validation.inserted_distinct_singular"
                                } else {
                                    "shell.validation.inserted_distinct_plural"
                                },
                                &[("count", &vals.len().to_string()), ("column", &lookup_word)],
                            ),
                        );
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Async fetch of column values for the value-lookup completion popup.
    /// Checks the in-memory cache first; on miss, queries the database and
    /// caches non-empty results. A foreign-key id column (`fk`) fetches the
    /// referenced table's (key, readable label) pairs, so the popup lists
    /// names and inserts ids; if that query fails it falls back to the raw
    /// distinct ids. Results reach the editor with the monotonic
    /// `request_token` so stale fetches are ignored.
    pub(super) fn run_value_completions_fetch(
        &mut self,
        tab_idx: usize,
        context: ValueContext,
        request_token: u64,
        partial: String,
        fk: Option<sqlly_intel::modelengine::intellisense::FkValuePivot>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.validation.connect_completion_values"),
            );
            cx.notify();
            return;
        };

        let Some(plain_query) = sqlly_services::distinct_values_sql_matching(
            profile.database_type,
            &context,
            &partial,
            VALUE_LOOKUP_FETCH_LIMIT,
        ) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.validation.table_column_unresolved"),
            );
            cx.notify();
            return;
        };
        let fk_query = fk.as_ref().map(|pivot| {
            let target = &pivot.target;
            let parts: Vec<&str> = [
                target.catalog.as_ref().map(|c| c.spelling()),
                target.namespace.as_ref().map(|n| n.spelling()),
                Some(target.name.spelling()),
            ]
            .into_iter()
            .flatten()
            .collect();
            sqlly_services::fk_values_sql_matching(
                profile.database_type,
                &parts,
                &pivot.key_column,
                &pivot.display_column,
                &partial,
                VALUE_LOOKUP_FETCH_LIMIT,
            )
        });

        let conn_id = profile.id.0.to_string();
        let database = profile.database.clone();
        let schema = context.schema.clone();
        let table = context.table.clone();
        let column = context.column.clone();

        let mut cache_key = value_lookup_cache_key(
            &conn_id,
            database.as_deref().unwrap_or(""),
            schema.as_deref(),
            table.as_deref().unwrap_or(""),
            column.as_deref().unwrap_or(""),
        );
        // Labeled FK pairs and raw ids for the same column never share an
        // entry (a LIKE lookup on the id column still wants the raw ids).
        if let Some(pivot) = &fk {
            cache_key.push_str(&format!(
                "|fk:{}:{}",
                pivot.key_column.to_ascii_lowercase(),
                pivot.display_column.to_ascii_lowercase()
            ));
        }
        // A server-side filtered fetch is its own cache entry: it is not
        // the column's full value set.
        if !partial.is_empty() {
            cache_key.push_str(&format!("|like:{}", partial.to_ascii_lowercase()));
        }

        if let Some(cached) = self.value_lookup_cache.get(&cache_key).cloned() {
            Self::lru_touch(&mut self.value_lookup_cache_order, &cache_key);
            let editor_entity = match self.tabs.get(tab_idx) {
                Some(t) => t.editor.clone(),
                None => return,
            };
            let truncated = cached.len() >= VALUE_LOOKUP_FETCH_LIMIT;
            editor_entity.update(cx, |e, cx| {
                e.apply_value_lookup_fetch(request_token, cached, partial, truncated, cx);
            });
            return;
        }

        let column_label = column.clone().unwrap_or_else(|| "?".to_string());
        let status_label = match &fk {
            Some(pivot) => crate::localization::format(
                "shell.validation.fk_status_label",
                &[
                    ("column", &column_label),
                    ("table", &pivot.target.name.spelling().to_string()),
                    ("display_column", &pivot.display_column),
                ],
            ),
            None => column_label.clone(),
        };
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.validation.lookup_progress",
                &[("label", &status_label)],
            ),
        );
        cx.notify();

        let editor_entity = match self.tabs.get(tab_idx) {
            Some(t) => t.editor.clone(),
            None => return,
        };
        // Same routing rule as `run_value_lookup`: the query runs on the
        // server over the profile's route.
        let services = self.services_for_profile(&profile);

        let (tx, rx) = tokio::sync::oneshot::channel::<
            Option<(Vec<sqlly_services::ValueLookupEntry>, bool)>,
        >();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    // FK pairs first; a failed pair query (a LOB label column
                    // an engine cannot ORDER BY, a permission gap on the
                    // referenced table) degrades to the raw ids.
                    if let Some(fk_query) = fk_query
                        && let Ok(handle) = services.queries.execute(&profile, &fk_query, 64).await
                        && let Some(pairs) = collect_key_label_pairs(handle).await
                    {
                        let _ = tx.send(Some((pairs, true)));
                        return;
                    }
                    let result = match services.queries.execute(&profile, &plain_query, 64).await {
                        Ok(handle) => collect_first_column(handle).await.map(|values| {
                            let entries = values
                                .into_iter()
                                .map(sqlly_services::ValueLookupEntry::plain)
                                .collect();
                            (entries, false)
                        }),
                        Err(_) => None,
                    };
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Some((vals, labeled))) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        // Only labeled results belong under the FK key; a
                        // fallback to raw ids is not cached there, so the
                        // next trigger retries the pairs.
                        if !vals.is_empty() && (labeled || !cache_key.contains("|fk:")) {
                            this.insert_value_lookup_cache(cache_key, vals.clone());
                        }
                        let count = vals.len();
                        let truncated = count >= VALUE_LOOKUP_FETCH_LIMIT;
                        editor_entity.update(cx, |e, cx| {
                            e.apply_value_lookup_fetch(request_token, vals, partial, truncated, cx);
                        });
                        let (level, message) = if count > 0 {
                            (
                                StatusLevel::Success,
                                crate::localization::format(
                                    "shell.validation.values_found",
                                    &[("count", &count.to_string()), ("label", &status_label)],
                                ),
                            )
                        } else {
                            (
                                StatusLevel::Info,
                                crate::localization::format(
                                    "shell.validation.no_values_found",
                                    &[("column", &column_label)],
                                ),
                            )
                        };
                        this.set_status(level, message);
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }
}

/// Collect (key, label) pairs — the first two columns of every row — from a
/// foreign-key value query, deduplicated by key. `None` when the query
/// errors, so the caller can fall back to the plain distinct ids.
async fn collect_key_label_pairs(
    handle: sqlly_services::QueryHandle,
) -> Option<Vec<sqlly_services::ValueLookupEntry>> {
    let mut events = handle.events;
    let mut out: Vec<sqlly_services::ValueLookupEntry> = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            sqlly_services::QueryEvent::Rows { rows, .. } => {
                push_key_label_rows(rows, &mut out);
            }
            sqlly_services::QueryEvent::SpillManifest { manifest } => {
                for set in &manifest.result_sets {
                    if let Some(rows) = sqlly_services::take_spilled_rows(set) {
                        push_key_label_rows(rows, &mut out);
                    }
                }
            }
            sqlly_services::QueryEvent::Error { .. } => return None,
            sqlly_services::QueryEvent::Completed { .. }
            | sqlly_services::QueryEvent::Cancelled => return Some(out),
            _ => {}
        }
    }
    Some(out)
}

/// Append each row's (key, label) pair, skipping NULL keys and keys already
/// present (a spilled set re-delivers its prefix). A NULL label falls back to
/// no label, so the key still shows.
fn push_key_label_rows(
    rows: Vec<Vec<sqlly_core::CellValue>>,
    out: &mut Vec<sqlly_services::ValueLookupEntry>,
) {
    for row in rows {
        let Some(key) = row.first().and_then(cell_display_text) else {
            continue;
        };
        if out.iter().any(|entry| entry.value == key) {
            continue;
        }
        let label = row.get(1).and_then(cell_display_text);
        out.push(sqlly_services::ValueLookupEntry { value: key, label });
    }
}

/// A cell as lookup text, or `None` for NULL.
fn cell_display_text(cell: &sqlly_core::CellValue) -> Option<String> {
    use sqlly_core::CellValue;
    Some(match cell {
        CellValue::Null => return None,
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(n) => n.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => format!("{b:?}"),
    })
}

#[cfg(test)]
mod value_pair_tests {
    use super::push_key_label_rows;
    use sqlly_core::CellValue;

    /// The FK lookup rows become (id, name) entries: NULL keys dropped, a
    /// re-delivered key kept once, a NULL name leaves the id unlabeled.
    #[test]
    fn key_label_rows_become_labeled_entries() {
        let mut out = Vec::new();
        push_key_label_rows(
            vec![
                vec![CellValue::I64(1), CellValue::String("Beverages".into())],
                vec![CellValue::Null, CellValue::String("orphan".into())],
                vec![CellValue::I64(2), CellValue::Null],
            ],
            &mut out,
        );
        push_key_label_rows(
            vec![vec![
                CellValue::I64(1),
                CellValue::String("Beverages".into()),
            ]],
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                sqlly_services::ValueLookupEntry {
                    value: "1".into(),
                    label: Some("Beverages".into()),
                },
                sqlly_services::ValueLookupEntry {
                    value: "2".into(),
                    label: None,
                },
            ]
        );
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{extract_table_reference, find_line_for_identifier, map_validation_result};
    use crate::editor::DiagnosticSeverity;
    use sqlly_ai::catalog::{
        SqlIntent, SqlIssueCode, SqlIssueSeverity, SqlValidationIssue, SqlValidationResult,
    };
    use sqlly_lang::sqlmodel::Span;

    #[test]
    fn map_validation_result_maps_severities_and_span_line() {
        let sql = "SELECT 1\nFROM dbo.Ghosts";
        let result = SqlValidationResult {
            is_valid: false,
            requires_confirmation: true,
            detected_intent: SqlIntent::Select,
            issues: vec![
                SqlValidationIssue {
                    code: SqlIssueCode::UnresolvedTable,
                    severity: SqlIssueSeverity::Error,
                    message: "unknown table".into(),
                    identifier: None,
                    object_keys: vec![],
                    span: Some(Span {
                        start: sql.find("Ghosts").unwrap(),
                        end: sql.len(),
                    }),
                },
                SqlValidationIssue {
                    code: SqlIssueCode::CrossDatabaseReference,
                    severity: SqlIssueSeverity::Warning,
                    message: "cross-db".into(),
                    identifier: Some("otherdb.dbo.t".into()),
                    object_keys: vec![],
                    span: None,
                },
                SqlValidationIssue {
                    code: SqlIssueCode::DestructiveRequiresConfirmation,
                    severity: SqlIssueSeverity::RequireConfirmation,
                    message: "confirm drop".into(),
                    identifier: None,
                    object_keys: vec![],
                    span: None,
                },
            ],
            referenced_objects: vec![],
            referenced_columns: vec![],
        };
        let diagnostics = map_validation_result(&result, sql);
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostics[0].line, 1);
        assert_eq!(diagnostics[1].severity, DiagnosticSeverity::Warning);
        assert_eq!(diagnostics[1].line, 0);
        assert_eq!(
            diagnostics[2].severity,
            DiagnosticSeverity::RequireConfirmation
        );
    }

    #[test]
    fn find_line_for_identifier_is_case_insensitive() {
        let sql = "SELECT *\nFROM dbo.Customers";
        assert_eq!(find_line_for_identifier(sql, "customers"), Some(1));
        assert_eq!(find_line_for_identifier(sql, "missing"), None);
    }

    #[test]
    fn extract_table_reference_returns_last_from_or_join() {
        let sql = "SELECT * FROM dbo.A JOIN dbo.B ON 1 = 1";
        assert_eq!(extract_table_reference(sql).as_deref(), Some("dbo.B"));
    }
}
