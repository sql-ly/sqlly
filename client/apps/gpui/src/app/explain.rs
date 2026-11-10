use super::*;

/// Drains a query stream and returns every ShowPlan XML document in it —
/// one per captured result set. `SET SHOWPLAN_XML ON` yields a single doc;
/// `SET STATISTICS XML ON` (actual plans) interleaves the query's own result
/// sets with one plan doc per statement, so cells are filtered to those
/// containing a `<ShowPlanXML` root. A server error (e.g. SHOWPLAN permission
/// denied) is captured verbatim so the pane can show the real reason instead
/// of a generic "no plan returned".
/// Assemble captured plan documents from the query events, per dialect:
/// SQL Server yields one ShowPlan XML document per plan row; PostgreSQL and
/// MySQL return one JSON document spread across text rows (joined by
/// newlines); SQLite returns `(id, parent, _, detail)` rows rendered as
/// `id|parent|detail` lines.
async fn collect_plan_documents(
    handle: sqlly_services::QueryHandle,
    dialect: sqlly_services::plans::PlanDialect,
) -> (Vec<String>, Option<String>) {
    use sqlly_core::CellValue;
    use sqlly_services::plans::PlanDialect;
    fn collect_docs(dialect: PlanDialect, rows: &[Vec<CellValue>], lines: &mut Vec<String>) {
        for row in rows {
            match dialect {
                PlanDialect::SqlServer => {
                    if let Some(CellValue::String(s)) = row.first()
                        && s.contains("<ShowPlanXML")
                    {
                        lines.push(s.clone());
                    }
                }
                PlanDialect::Sqlite => {
                    let cell = |i: usize| match row.get(i) {
                        Some(CellValue::I64(n)) => n.to_string(),
                        Some(CellValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    lines.push(format!("{}|{}|{}", cell(0), cell(1), cell(3)));
                }
                // Oracle: one tab-separated PLAN_TABLE row per line, in the
                // capture's single text column.
                PlanDialect::Postgres
                | PlanDialect::MySql
                | PlanDialect::MariaDb
                | PlanDialect::ClickHouse
                | PlanDialect::Oracle => {
                    if let Some(CellValue::String(s)) = row.first() {
                        lines.push(s.clone());
                    }
                }
                // `(explain_key, explain_value)`: the JSON document is the
                // second column.
                PlanDialect::DuckDb => {
                    if let Some(CellValue::String(s)) = row.get(1) {
                        lines.push(s.clone());
                    }
                }
            }
        }
    }
    let mut events = handle.events;
    let mut docs: Vec<String> = Vec::new();
    let mut server_error: Option<String> = None;
    while let Some(event) = events.recv().await {
        match event {
            sqlly_services::QueryEvent::Rows { rows, .. } => {
                collect_docs(dialect, &rows, &mut docs);
            }
            sqlly_services::QueryEvent::SpillManifest { manifest } => {
                // A plan document wider than the executor's byte budget is
                // withheld from `Rows` and exists only in the spill file.
                // Every set is drained so the files are deleted even when the
                // document was already found in the streamed prefix.
                for set in &manifest.result_sets {
                    let Some(rows) = sqlly_services::take_spilled_rows(set) else {
                        continue;
                    };
                    collect_docs(dialect, &rows, &mut docs);
                }
            }
            sqlly_services::QueryEvent::Error { message } => {
                server_error = Some(message);
                break;
            }
            sqlly_services::QueryEvent::Completed { .. }
            | sqlly_services::QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    // ShowPlan XML arrives one full document per collected row; the other
    // dialects return ONE document spread across rows — reassemble it.
    if !matches!(dialect, PlanDialect::SqlServer) && !docs.is_empty() {
        docs = vec![docs.join("\n")];
    }
    (docs, server_error)
}

/// A popup-menu entry that captures a plan. When `unavailable` carries the
/// plan capability's reason the entry is disabled and the reason is printed
/// under the label (gpui-component menu items have no tooltip), so the menu
/// explains itself instead of silently doing nothing.
pub(super) fn gated_plan_menu_item(
    label: impl Into<SharedString>,
    unavailable: Option<String>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> PopupMenuItem {
    let label = label.into();
    let Some(reason) = unavailable else {
        return PopupMenuItem::new(label).on_click(on_click);
    };
    PopupMenuItem::element(move |_window, _cx| {
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(2.0)))
            .child(label.clone())
            .child(
                div()
                    .max_w(px(Theme::scaled(280.0)))
                    .whitespace_normal()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(reason.clone()),
            )
    })
    .disabled(true)
}

struct PlanCaptureRequest {
    profile: sqlly_core::ConnectionProfile,
    results: Entity<crate::results::ResultsView>,
    kind: crate::execution_plan_view::PlanKind,
    plan_dialect: sqlly_services::plans::PlanDialect,
    opts: sqlly_services::plans::ExplainOptions,
    plan_sql: String,
    sql: String,
    result_label: String,
}

impl AppView {
    pub(super) fn handle_execution_plan_viewer(
        &mut self,
        _action: &ExecutionPlanViewer,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.explain_active_query(cx);
    }

    pub(super) fn handle_estimated_plan_viewer(
        &mut self,
        _action: &EstimatedPlanViewer,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.explain_estimated_query(cx);
    }

    /// Why the Explain affordance is unavailable right now, or `None` when it
    /// can run. Drives the toolbar button's disabled tooltip and the status
    /// message shown when the shortcut fires without a usable connection.
    pub(super) fn execution_plan_unavailable_reason(
        &self,
        kind: crate::execution_plan_view::PlanKind,
    ) -> Option<String> {
        let Some(profile) = self.active_profile.as_ref() else {
            return Some(crate::localization::text("shell.plans.connect_database"));
        };
        crate::capabilities::plan_unavailable_reason(profile.database_type, kind)
    }

    /// Push the active engine's plan-capture gate into every tab's results
    /// view, so the plan pane's Re-capture button and empty state reflect the
    /// connection. Called each frame; results views only notify on change.
    pub(super) fn sync_plan_capture_gate(&mut self, cx: &mut Context<Self>) {
        let gate = crate::capabilities::PlanCaptureGate::for_engine(
            self.active_profile.as_ref().map(|p| p.database_type),
        );
        for tab in &self.tabs {
            if tab.results.read(cx).plan_capture_gate() != &gate {
                let gate = gate.clone();
                tab.results
                    .update(cx, |results, cx| results.set_plan_capture_gate(gate, cx));
            }
        }
    }

    /// Status-bar fallback for the (capability-guarded, so unexpected) case
    /// where an engine reports plans available but has no plan dialect —
    /// never leave a plan shortcut silently doing nothing.
    fn report_missing_plan_dialect(
        &mut self,
        db: sqlly_lang::sqlmodel::DatabaseType,
        cx: &mut Context<Self>,
    ) {
        self.set_status(
            StatusLevel::Info,
            crate::localization::format(
                "shell.plans.unsupported_engine",
                &[("engine", db.display_name())],
            ),
        );
        cx.notify();
    }

    /// Capture the active editor's ACTUAL execution plan (the statement
    /// runs). ⌘L and the Explain button land here.
    pub(super) fn explain_active_query(&mut self, cx: &mut Context<Self>) {
        self.explain_query_kind(crate::execution_plan_view::PlanKind::Actual, cx);
    }

    /// Capture the ESTIMATED plan — the optimizer is asked, the statement
    /// never executes. SQL Server captures via batch-isolated SHOWPLAN_XML;
    /// PostgreSQL via EXPLAIN without ANALYZE; MySQL/SQLite plan captures
    /// never execute regardless.
    pub(super) fn explain_estimated_query(&mut self, cx: &mut Context<Self>) {
        self.explain_query_kind(crate::execution_plan_view::PlanKind::Estimated, cx);
    }

    /// Capture the active editor's execution plan into the results-area plan
    /// pane, as `kind` (estimated vs actual).
    pub(super) fn explain_query_kind(
        &mut self,
        kind: crate::execution_plan_view::PlanKind,
        cx: &mut Context<Self>,
    ) {
        use crate::execution_plan_view::PlanKind;

        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.plans.connect_server"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = self.execution_plan_unavailable_reason(kind) {
            self.set_status(StatusLevel::Info, reason);
            cx.notify();
            return;
        }
        let Some(tab) = self.tabs.get(self.active_tab) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.plans.no_active_tab"),
            );
            cx.notify();
            return;
        };
        let sql = tab.editor.read(cx).text().to_string();
        if sql.trim().is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.plans.no_sql"),
            );
            cx.notify();
            return;
        }
        let results = tab.results.clone();
        // A capture is already running for this tab — launching another would
        // race the same pane. The in-flight capture wins.
        if results.read(cx).is_capturing_execution_plan(cx) {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.plans.capture_in_progress"),
            );
            cx.notify();
            return;
        }
        let Some(plan_dialect) = sqlly_services::plan_dialect(profile.database_type) else {
            self.report_missing_plan_dialect(profile.database_type, cx);
            return;
        };
        let opts = self.effective_explain_options(&profile, kind);
        let plan_sql = sqlly_services::plans::explain_sql_with_options(
            plan_dialect,
            &opts,
            kind == PlanKind::Actual,
            &sql,
        );
        self.capture_plan_sql(
            PlanCaptureRequest {
                profile,
                results,
                kind,
                plan_dialect,
                opts,
                plan_sql,
                sql,
                result_label: match kind {
                    PlanKind::Actual => crate::localization::text("shell.plans.actual_label"),
                    PlanKind::Estimated => crate::localization::text("shell.plans.estimated_label"),
                },
            },
            cx,
        );
    }

    pub(crate) fn explain_with_preset(
        &mut self,
        preset: sqlly_services::plans::ExplainPreset,
        cx: &mut Context<Self>,
    ) {
        use crate::execution_plan_view::PlanKind;
        match preset {
            sqlly_services::plans::ExplainPreset::Actual => self.explain_active_query(cx),
            sqlly_services::plans::ExplainPreset::Estimated => self.explain_estimated_query(cx),
            sqlly_services::plans::ExplainPreset::Profile => {
                let Some(profile) = self.active_profile.clone() else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.plans.profile_connect_server"),
                    );
                    cx.notify();
                    return;
                };
                if let Some(reason) = self.execution_plan_unavailable_reason(PlanKind::Actual) {
                    self.set_status(StatusLevel::Info, reason);
                    cx.notify();
                    return;
                }
                let Some(tab) = self.tabs.get(self.active_tab) else {
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::text("shell.plans.profile_no_active_tab"),
                    );
                    cx.notify();
                    return;
                };
                let sql = tab.editor.read(cx).text().to_string();
                if sql.trim().is_empty() {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.plans.profile_no_sql"),
                    );
                    cx.notify();
                    return;
                }
                if let Some(reason) = crate::explain_options_popover::profile_block_reason(&sql) {
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.plans.profile_select_only",
                            &[("reason", &reason)],
                        ),
                    );
                    cx.notify();
                    return;
                }
                let Some(plan_dialect) = sqlly_services::plan_dialect(profile.database_type) else {
                    self.report_missing_plan_dialect(profile.database_type, cx);
                    return;
                };
                let version = self
                    .pending
                    .server_versions
                    .get(&profile.id.0.to_string())
                    .copied()
                    .flatten();
                let opts =
                    sqlly_services::plans::ExplainOptions::profile_defaults(version, plan_dialect);
                let results = tab.results.clone();
                if results.read(cx).is_capturing_execution_plan(cx) {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.plans.profile_capture_in_progress"),
                    );
                    cx.notify();
                    return;
                }
                let plan_sql = sqlly_services::plans::explain_sql_with_options(
                    plan_dialect,
                    &opts,
                    true,
                    &sql,
                );
                self.capture_plan_sql(
                    PlanCaptureRequest {
                        profile,
                        results,
                        kind: PlanKind::Actual,
                        plan_dialect,
                        opts,
                        plan_sql,
                        sql,
                        result_label: crate::localization::text("shell.plans.profile_label"),
                    },
                    cx,
                );
            }
        }
    }

    fn capture_plan_sql(&mut self, request: PlanCaptureRequest, cx: &mut Context<Self>) {
        use crate::execution_plan_view::PlanKind;
        let PlanCaptureRequest {
            profile,
            results,
            kind,
            plan_dialect,
            opts,
            plan_sql,
            sql,
            result_label,
        } = request;
        let kind_is_actual = kind == PlanKind::Actual;
        results.update(cx, |results, cx| {
            results.begin_execution_plan_capture(kind, cx);
        });
        self.show_results();
        let executes = opts.executes_statement(plan_dialect, kind_is_actual)
            && !sqlly_intel::query::safety::classify_select_only(&sql).select_only;
        let capturing = crate::localization::format(
            if executes {
                "shell.plans.capturing_executes"
            } else {
                "shell.plans.capturing"
            },
            &[("plan", &result_label)],
        );
        self.set_status(StatusLevel::Busy, capturing);
        cx.notify();

        let services = self.services_for_profile(&profile);
        // The runtime task returns failures unworded; the UI thread words
        // them (translation state is per thread).
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<String>, PlanCaptureFailure>>();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = match services.queries.execute(&profile, &plan_sql, 64).await {
                        Ok(handle) => {
                            let (docs, server_error) =
                                collect_plan_documents(handle, plan_dialect).await;
                            if !docs.is_empty() {
                                Ok(docs)
                            } else if let Some(message) = server_error {
                                Err(PlanCaptureFailure::Server(message))
                            } else {
                                Err(PlanCaptureFailure::NoneReturned)
                            }
                        }
                        Err(e) => Err(PlanCaptureFailure::Capture(e.message())),
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
            if let Ok(result) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| match result {
                        Ok(docs) => {
                            let mut plans = Vec::new();
                            let mut parse_error = None;
                            for (index, doc) in docs.iter().enumerate() {
                                match sqlly_services::plans::parse_captured_plan(plan_dialect, doc)
                                {
                                    Ok(mut parsed) => plans.append(&mut parsed),
                                    Err(e) => {
                                        parse_error = Some(if docs.len() > 1 {
                                            crate::localization::format(
                                                "shell.plans.document_parse_error",
                                                &[
                                                    ("index", &(index + 1).to_string()),
                                                    ("total", &docs.len().to_string()),
                                                    ("error", &e.to_string()),
                                                ],
                                            )
                                        } else {
                                            e.to_string()
                                        });
                                        break;
                                    }
                                }
                            }
                            if let Some(error) = parse_error {
                                this.set_status(
                                    StatusLevel::Error,
                                    crate::localization::text("shell.plans.parse_failed_status"),
                                );
                                results.update(cx, |results, cx| {
                                    results.fail_execution_plan(
                                        kind,
                                        crate::localization::format(
                                            "shell.plans.parse_failed_viewer",
                                            &[("error", &error)],
                                        ),
                                        cx,
                                    );
                                });
                            } else {
                                let statement_count = plans.len();
                                results.update(cx, |results, cx| {
                                    results.set_execution_plan_for_context(
                                        kind,
                                        sql.clone(),
                                        plans,
                                        cx,
                                    );
                                });
                                let finding_count = results
                                    .read(cx)
                                    .execution_plan_finding_count(cx)
                                    .unwrap_or(0);
                                let summary_key = match (statement_count == 1, finding_count == 1) {
                                    (true, true) => "shell.plans.summary_singular",
                                    (true, false) => "shell.plans.summary_statement_singular",
                                    (false, true) => "shell.plans.summary_finding_singular",
                                    (false, false) => "shell.plans.summary_plural",
                                };
                                this.set_status(
                                    StatusLevel::Success,
                                    crate::localization::format(
                                        summary_key,
                                        &[
                                            ("plan", &result_label),
                                            ("statement_count", &statement_count.to_string()),
                                            ("finding_count", &finding_count.to_string()),
                                        ],
                                    ),
                                );
                            }
                        }
                        Err(failure) => {
                            let e = failure.text();
                            this.set_status(StatusLevel::Error, e.clone());
                            results.update(cx, |results, cx| {
                                results.fail_execution_plan(kind, e, cx);
                            });
                        }
                    })
                    .ok();
            }
        })
        .detach();
    }

    pub(crate) fn explain_popover_snapshot(
        &self,
    ) -> crate::explain_options_popover::ExplainPopoverSnapshot {
        use crate::explain_options_popover::{ExplainPopoverSnapshot, version_line};
        let Some(profile) = self.active_profile.as_ref() else {
            return ExplainPopoverSnapshot {
                connected: false,
                dialect: None,
                version: None,
                version_text: crate::localization::text("shell.plans.connect_options").into(),
                options: sqlly_services::plans::ExplainOptions::actual_defaults(),
                actual_unavailable: None,
                estimated_unavailable: None,
            };
        };
        use crate::capabilities::plan_unavailable_reason;
        use crate::execution_plan_view::PlanKind;
        let db = profile.database_type;
        let dialect = sqlly_services::plan_dialect(db);
        let cached = self.pending.server_versions.get(&profile.id.0.to_string());
        let estimated_unavailable = plan_unavailable_reason(db, PlanKind::Estimated);
        ExplainPopoverSnapshot {
            connected: true,
            dialect,
            version: cached.copied().flatten(),
            // No plan dialect: the popover shows why plans are disabled here
            // instead of options that could never apply.
            version_text: dialect.map(|d| version_line(d, cached)).unwrap_or_else(|| {
                estimated_unavailable
                    .clone()
                    .unwrap_or_else(|| crate::localization::text("shell.plans.version_unknown"))
                    .into()
            }),
            options: self.effective_explain_options(profile, PlanKind::Actual),
            actual_unavailable: plan_unavailable_reason(db, PlanKind::Actual),
            estimated_unavailable,
        }
    }

    pub(crate) fn effective_explain_options(
        &self,
        profile: &sqlly_core::ConnectionProfile,
        kind: crate::execution_plan_view::PlanKind,
    ) -> sqlly_services::plans::ExplainOptions {
        use crate::execution_plan_view::PlanKind;
        let id = profile.id.0.to_string();
        let dialect = sqlly_services::plan_dialect(profile.database_type);
        let mut opts = self
            .pending
            .explain_prefs
            .options_for(&id)
            .unwrap_or_else(|| match kind {
                PlanKind::Actual => sqlly_services::plans::ExplainOptions::actual_defaults(),
                PlanKind::Estimated => sqlly_services::plans::ExplainOptions::estimated_defaults(),
            });
        if let Some(dialect) = dialect {
            let version = self.pending.server_versions.get(&id).copied().flatten();
            opts = opts.clamped(dialect, version);
        }
        opts
    }

    pub(crate) fn set_explain_flag(&mut self, key: &str, value: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        let id = profile.id.0.to_string();
        let dialect = sqlly_services::plan_dialect(profile.database_type);
        let mut opts =
            self.effective_explain_options(&profile, crate::execution_plan_view::PlanKind::Actual);
        match key {
            "analyze" => {
                opts.pg.analyze = value;
                opts.mysql.analyze = value;
            }
            "verbose" => opts.pg.verbose = value,
            "costs" => opts.pg.costs = value,
            "buffers" => opts.pg.buffers = value,
            "timing" => opts.pg.timing = value,
            "summary" => opts.pg.summary = value,
            "settings" => opts.pg.settings = value,
            "wal" => opts.pg.wal = value,
            "generic_plan" => opts.pg.generic_plan = value,
            "memory" => opts.pg.memory = value,
            _ => {}
        }
        if let Some(dialect) = dialect {
            let version = self.pending.server_versions.get(&id).copied().flatten();
            opts = opts.clamped(dialect, version);
        }
        self.pending.explain_prefs.set_options(&id, opts);
        self.pending.explain_prefs.save();
        cx.notify();
    }

    pub(crate) fn reset_explain_options(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.active_profile.as_ref() else {
            return;
        };
        let id = profile.id.0.to_string();
        self.pending.explain_prefs.set_options(
            &id,
            sqlly_services::plans::ExplainOptions::actual_defaults(),
        );
        self.pending.explain_prefs.save();
        cx.notify();
    }

    pub(crate) fn ensure_server_version(
        &mut self,
        profile: &sqlly_core::ConnectionProfile,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id.0.to_string();
        if self.pending.server_versions.contains_key(&id) {
            return;
        }
        let Some(dialect) = sqlly_services::plan_dialect(profile.database_type) else {
            self.pending.server_versions.insert(id, None);
            return;
        };
        let probe_sql = version_probe_sql(dialect);
        let services = self.services_for_profile(profile);
        let profile = profile.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let cell = match services.queries.execute(&profile, probe_sql, 1).await {
                        Ok(handle) => collect_first_cell(handle).await,
                        Err(_) => None,
                    };
                    let _ = tx.send(cell);
                })
                .await
                .ok();
        })
        .detach();
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok(cell) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.pending.server_versions.insert(
                            id,
                            cell.as_deref()
                                .and_then(sqlly_services::plans::ServerVersion::parse),
                        );
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    pub(super) fn handle_explain_profile(
        &mut self,
        _action: &ExplainProfile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.explain_with_preset(sqlly_services::plans::ExplainPreset::Profile, cx);
    }

    pub(super) fn handle_open_plan_file(
        &mut self,
        _action: &OpenPlanFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_plan_file(cx);
    }

    pub(super) fn handle_paste_plan(
        &mut self,
        _action: &PastePlan,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste_plan(cx);
    }

    pub(super) fn open_plan_file(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.plans.open_file_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(crate::localization::text("shell.plans.open_file_prompt").into()),
            });
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let Ok(Ok(Some(paths))) = rx.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let label = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::localization::text("shell.plans.file_fallback"));
                let result = std::fs::read_to_string(&path).map_err(|error| {
                    crate::localization::format(
                        "shell.plans.file_read_failed",
                        &[
                            ("path", &path.display().to_string()),
                            ("error", &error.to_string()),
                        ],
                    )
                });
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| match result {
                        Ok(doc) => this.import_plan_document(doc, label, cx),
                        Err(e) => {
                            this.set_status(StatusLevel::Error, e);
                            cx.notify();
                        }
                    })
                    .ok();
            })
            .detach();
        }
    }

    pub(super) fn paste_plan(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.plans.clipboard_empty"),
            );
            cx.notify();
            return;
        };
        if text.trim().is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.plans.clipboard_empty"),
            );
            cx.notify();
            return;
        }
        self.import_plan_document(
            text,
            crate::localization::text("shell.plans.clipboard_source"),
            cx,
        );
    }

    pub(super) fn import_plan_document(
        &mut self,
        doc: String,
        source_label: String,
        cx: &mut Context<Self>,
    ) {
        // Terminal wrappers (psql's `+` continuations, mysql table borders)
        // come off first; the same reader backs the online plan viewer.
        let doc = sqlly_services::plans::clean_plan_text(&doc);
        let Some(dialect) = sniff_plan_dialect(&doc) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.plans.unrecognised_format"),
            );
            cx.notify();
            return;
        };
        match sqlly_services::plans::parse_plan_document(dialect, &doc) {
            Ok(plans) => {
                if self.tabs.is_empty() {
                    self.new_tab(cx, None);
                }
                let Some(tab) = self.tabs.get(self.active_tab) else {
                    return;
                };
                let results = tab.results.clone();
                self.show_results();
                let statement_count = plans.len();
                results.update(cx, |results, cx| {
                    results.set_execution_plan_for_context(
                        crate::execution_plan_view::PlanKind::Estimated,
                        crate::localization::format(
                            "shell.plans.imported_context",
                            &[("source", &source_label)],
                        ),
                        plans,
                        cx,
                    );
                });
                let finding_count = results
                    .read(cx)
                    .execution_plan_finding_count(cx)
                    .unwrap_or(0);
                let summary_key = match (statement_count == 1, finding_count == 1) {
                    (true, true) => "shell.plans.import_summary_singular",
                    (true, false) => "shell.plans.import_summary_statement_singular",
                    (false, true) => "shell.plans.import_summary_finding_singular",
                    (false, false) => "shell.plans.import_summary_plural",
                };
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        summary_key,
                        &[
                            ("dialect", &format!("{dialect:?}")),
                            ("statement_count", &statement_count.to_string()),
                            ("finding_count", &finding_count.to_string()),
                        ],
                    ),
                );
                cx.notify();
            }
            Err(e) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.plans.import_failed",
                        &[("error", &e.to_string())],
                    ),
                );
                cx.notify();
            }
        }
    }
}

fn version_probe_sql(dialect: sqlly_services::plans::PlanDialect) -> &'static str {
    match dialect {
        sqlly_services::plans::PlanDialect::Postgres => "SELECT current_setting('server_version')",
        sqlly_services::plans::PlanDialect::MySql | sqlly_services::plans::PlanDialect::MariaDb => {
            "SELECT VERSION()"
        }
        sqlly_services::plans::PlanDialect::SqlServer => {
            "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128))"
        }
        sqlly_services::plans::PlanDialect::Sqlite => "SELECT sqlite_version()",
        sqlly_services::plans::PlanDialect::DuckDb
        | sqlly_services::plans::PlanDialect::ClickHouse => "SELECT version()",
        sqlly_services::plans::PlanDialect::Oracle => {
            "SELECT version FROM product_component_version WHERE product LIKE 'Oracle%' FETCH FIRST 1 ROWS ONLY"
        }
    }
}

async fn collect_first_cell(handle: sqlly_services::QueryHandle) -> Option<String> {
    use sqlly_core::CellValue;
    let mut events = handle.events;
    while let Some(event) = events.recv().await {
        match event {
            sqlly_services::QueryEvent::Rows { rows, .. } => {
                if let Some(cell) = rows.first().and_then(|row| row.first()) {
                    return Some(match cell {
                        CellValue::String(s) => s.clone(),
                        CellValue::I64(n) => n.to_string(),
                        CellValue::F64(n) => n.to_string(),
                        CellValue::Bool(b) => b.to_string(),
                        CellValue::Null => String::new(),
                        CellValue::Bytes(_) => String::new(),
                    });
                }
            }
            sqlly_services::QueryEvent::Error { .. }
            | sqlly_services::QueryEvent::Completed { .. }
            | sqlly_services::QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    None
}

/// Sniff a pasted/opened plan document's engine with the shared reader
/// (`sqlly_services::plans::sniff_plan_document`). Pure; returns None for
/// anything it does not recognize, including plans in a form no parser reads.
pub(crate) fn sniff_plan_dialect(doc: &str) -> Option<sqlly_services::plans::PlanDialect> {
    match sqlly_services::plans::sniff_plan_document(doc) {
        sqlly_services::plans::PlanSniff::Dialect(dialect) => Some(dialect),
        _ => None,
    }
}

/// Why a plan capture produced no documents, worded on the UI thread by
/// [`PlanCaptureFailure::text`] because the capture runs on the runtime.
enum PlanCaptureFailure {
    /// Server error text, shown as the server sent it.
    Server(String),
    NoneReturned,
    Capture(sqlly_core::DisplayMessage),
}

impl PlanCaptureFailure {
    fn text(&self) -> String {
        match self {
            Self::Server(message) => message.clone(),
            Self::NoneReturned => crate::localization::text("shell.plans.server_returned_none"),
            Self::Capture(error) => crate::localization::format(
                "shell.plans.capture_failed",
                &[("error", crate::localization::message(error))],
            ),
        }
    }
}

#[cfg(test)]
mod sniff_tests {
    use super::sniff_plan_dialect;
    use sqlly_services::plans::PlanDialect;

    #[test]
    fn sniff_showplan() {
        assert_eq!(
            sniff_plan_dialect(
                "<ShowPlanXML xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/showplan\">"
            ),
            Some(PlanDialect::SqlServer)
        );
    }

    #[test]
    fn sniff_pg_json() {
        assert_eq!(
            sniff_plan_dialect("[{\"Plan\": {\"Node Type\": \"Result\"}}]"),
            Some(PlanDialect::Postgres)
        );
    }

    #[test]
    fn sniff_clickhouse_json_before_postgres() {
        assert_eq!(
            sniff_plan_dialect(
                "[{\"Plan\": {\"Node Type\": \"Expression\", \"Node Id\": \"Expression_2\"}}]"
            ),
            Some(PlanDialect::ClickHouse)
        );
    }

    #[test]
    fn sniff_mysql_json_and_tree() {
        assert_eq!(
            sniff_plan_dialect("{\"query_block\": {\"select_id\": 1}}"),
            Some(PlanDialect::MySql)
        );
        assert_eq!(
            sniff_plan_dialect("-> Nested loop\n    -> Table scan"),
            Some(PlanDialect::MySql)
        );
    }

    #[test]
    fn sniff_sqlite_rows() {
        assert_eq!(
            sniff_plan_dialect("2|0|SCAN TABLE t"),
            Some(PlanDialect::Sqlite)
        );
    }

    #[test]
    fn sniff_garbage_is_none() {
        assert_eq!(sniff_plan_dialect("hello world"), None);
        assert_eq!(sniff_plan_dialect("{}"), None);
        assert_eq!(sniff_plan_dialect("[{}]"), None);
    }
}
