use super::*;

impl AppView {
    // ── Procedure mode (deploy + invoke loop) ────────────────────────────────
    //
    // The pure parsing/batch logic lives in `sqlly_intel::query::procedure`; the per-tab
    // state, plan building, and step orchestration live in
    // `crate::procedure_mode`. This block owns the GPUI wiring: the toolbar
    // toggle, the run dispatch, the event pump, and the parameter strip.

    /// Flip Procedure mode for the active tab. Turning it on parses the
    /// current buffer immediately so the strip renders on the next frame.
    /// Which procedure grammar the active connection speaks. PostgreSQL gets
    /// its own parse/deploy/invoke pipeline; everything else uses T-SQL (the
    /// capability gate keeps the toggle disabled on unported engines).
    pub(super) fn active_proc_dialect(&self) -> crate::procedure_mode::ProcDialect {
        self.active_profile
            .as_ref()
            .map(|profile| sqlly_services::proc_dialect(profile.database_type))
            .unwrap_or_default()
    }

    pub(super) fn toggle_proc_mode(&mut self, cx: &mut Context<Self>) {
        // Engine gate: the deploy/invoke pipeline is T-SQL. The toolbar
        // button disables itself, but the command palette and keybinding
        // paths land here too.
        if let Some(reason) = crate::capabilities::procedure_mode(self.active_db_type()).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        let tab_idx = self.active_tab;
        let proc_dialect = self.active_proc_dialect();
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return;
        };
        tab.proc_mode.enabled = !tab.proc_mode.enabled;
        let enabled = tab.proc_mode.enabled;
        if enabled {
            let script = tab.editor.read(cx).text().to_string();
            tab.proc_mode.refresh_from_script_for(&script, proc_dialect);
            tab.proc_mode.return_code = None;
            for row in &mut tab.proc_mode.rows {
                row.output_value = None;
            }
        }
        self.toolbar.procedure_mode = enabled;
        cx.notify();
    }

    /// Dispatch the deploy + invoke sequence for a successfully parsed
    /// procedure script. Validation of the strip's values happens here, on
    /// the UI thread, before anything is sent to the server: the first
    /// invalid value marks its row and blocks the run with a status message.
    pub(super) fn execute_proc_query(
        &mut self,
        tab_idx: usize,
        script: String,
        header: sqlly_intel::query::procedure::ProcHeader,
        profile: ConnectionProfile,
        results_entity: Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        let plan = {
            let Some(tab) = self.tabs.get_mut(tab_idx) else {
                return;
            };
            // Pull the latest text out of the InputStates (subscriptions keep
            // `last_value` current, but a value typed into a still-focused
            // input races the F5 keypress — read canonical state at dispatch).
            tab.proc_mode.sync_values_from_inputs(cx);
            let rollback = tab.proc_mode.rollback;
            for row in &mut tab.proc_mode.rows {
                row.validation_error = None;
                row.output_value = None;
            }
            tab.proc_mode.return_code = None;
            let built = match (tab.proc_mode.dialect, tab.proc_mode.pg_parsed.as_ref()) {
                (crate::procedure_mode::ProcDialect::Postgres, Some(pg_header)) => {
                    crate::procedure_mode::build_pg_plan(
                        &script,
                        pg_header,
                        &tab.proc_mode.rows,
                        rollback,
                    )
                }
                _ => crate::procedure_mode::build_plan(
                    &script,
                    &header,
                    &tab.proc_mode.rows,
                    rollback,
                ),
            };
            match built {
                Ok(plan) => plan,
                Err((row, message)) => {
                    let name = tab
                        .proc_mode
                        .rows
                        .get(row)
                        .map(|r| r.name.clone())
                        .unwrap_or_default();
                    if let Some(r) = tab.proc_mode.rows.get_mut(row) {
                        r.validation_error = Some(message.clone());
                    }
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.procedure.parameter_invalid",
                            &[("name", &name), ("error", &message)],
                        ),
                    );
                    cx.notify();
                    return;
                }
            }
        };
        self.spawn_proc_run(tab_idx, plan, profile, results_entity, cx);
    }

    /// Spawn the step orchestrator on the tokio runtime and the UI pump that
    /// applies its updates: forwarded events land in the results view (gated
    /// on the run stamp, exactly like `spawn_query_execution`), the current
    /// step's task id is parked on the tab for Cancel, peeled sentinel values
    /// land in the strip, and the terminal update sets the status line.
    fn spawn_proc_run(
        &mut self,
        tab_idx: usize,
        plan: crate::procedure_mode::ProcRunPlan,
        profile: ConnectionProfile,
        results_entity: Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        // Seal a Run version at dispatch (same policy as plain queries).
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.query_history_prefs.enabled {
                let now = chrono::Utc::now().timestamp_millis();
                let sealed = self.history_store_for_tab(tab_idx).and_then(|store| {
                    match store.seal(
                        crate::query_version_history::VersionKind::Run,
                        &plan.script,
                        now,
                    ) {
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

        let qualified_name = plan.qualified_name.clone();
        let services = self.services_for_profile(&profile);
        let row_edit_block = self.row_edit_connection_block();
        let run = results_entity.update(cx, |r, cx| {
            r.set_query_sql(&plan.script, cx);
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
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format("shell.procedure.deploying", &[("name", &qualified_name)]),
        );
        self.pending.mutation_review = None;

        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.proc_mode.running = true;
        }

        let tab_id = self
            .tabs
            .get(tab_idx)
            .map(|tab| tab.id.clone())
            .unwrap_or_default();

        // Supersede any prior run on this tab (same semantics as plain Run).
        // Both the prior-run cancel and the deploy/invoke sequence follow the
        // profile's route — a Via-Relay proc run deploys over the relay, and
        // its superseded run must be cancelled there too.
        if let Some(prior) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|t| t.active_query_id.take())
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

        let queries = self.services_for_profile(&profile).queries;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::procedure_mode::ProcRunUpdate>(64);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    crate::procedure_mode::run_proc_sequence(queries, profile, plan, tx).await;
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
            while let Some(update) = rx.recv().await {
                let mut cx = async_cx.clone();
                let keep_going = weak_self
                    .update(&mut cx, |this, cx| {
                        if results_entity.read(cx).run_generation() != run {
                            return false;
                        }
                        let Some(tab_idx) = this.tab_index_by_id(&tab_id_for_pump) else {
                            return false;
                        };
                        match update {
                            crate::procedure_mode::ProcRunUpdate::Event(event) => {
                                let mut error_message: Option<String> = None;
                                if let sqlly_services::QueryEvent::Error { ref message } = event {
                                    log.log_error("query", message);
                                    error_message = Some(message.clone());
                                }
                                results_entity
                                    .update(cx, |r, cx| r.handle_event_for_run(run, event, cx));
                                this.log_console.update(cx, |view, cx| view.refresh(cx));
                                if let Some(message) = error_message {
                                    this.arm_firewall_prompt(&message, None, cx);
                                }
                            }
                            crate::procedure_mode::ProcRunUpdate::TaskId(task_id) => {
                                if let Some(tab) = this.tabs.get_mut(tab_idx) {
                                    tab.active_query_id = Some(task_id);
                                }
                            }
                            crate::procedure_mode::ProcRunUpdate::Outputs {
                                return_code,
                                values,
                            } => {
                                if let Some(tab) = this.tabs.get_mut(tab_idx) {
                                    tab.proc_mode.return_code = return_code;
                                    for (name, value) in values {
                                        if let Some(row) = tab
                                            .proc_mode
                                            .rows
                                            .iter_mut()
                                            .find(|r| r.name.eq_ignore_ascii_case(&name))
                                        {
                                            row.output_value = Some(value);
                                        }
                                    }
                                }
                                cx.notify();
                            }
                            crate::procedure_mode::ProcRunUpdate::Finished { ok, status } => {
                                if let Some(tab) = this.tabs.get_mut(tab_idx) {
                                    tab.active_query_id = None;
                                    tab.proc_mode.running = false;
                                }
                                this.set_status(
                                    if ok {
                                        StatusLevel::Success
                                    } else {
                                        StatusLevel::Error
                                    },
                                    crate::localization::message(status),
                                );
                                cx.notify();
                                return false;
                            }
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
            // Channel closed without a Finished update (executor dropped):
            // make sure the tab never stays in the running state.
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    if results_entity.read(cx).run_generation() == run
                        && let Some(tab_idx) = this.tab_index_by_id(&tab_id_for_pump)
                        && let Some(tab) = this.tabs.get_mut(tab_idx)
                        && tab.proc_mode.running
                    {
                        tab.active_query_id = None;
                        tab.proc_mode.running = false;
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Reconcile the strip's rows with the latest parse, preserving values by
    /// parameter name (a row survives re-parse only when its name, type, and
    /// output flag all still match). Runs at render time because row inputs
    /// are `InputState` entities, which need a `&mut Window` to create.
    fn sync_proc_rows(&mut self, tab_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return;
        };
        if tab.proc_mode.synced_revision == tab.proc_mode.parse_revision {
            return;
        }
        tab.proc_mode.synced_revision = tab.proc_mode.parse_revision;
        let Some(Ok(header)) = tab.proc_mode.parsed.clone() else {
            tab.proc_mode.rows.clear();
            return;
        };
        let mut rows: Vec<crate::procedure_mode::ProcParamRow> =
            Vec::with_capacity(header.params.len());
        let mut old_rows = std::mem::take(&mut tab.proc_mode.rows);
        for param in &header.params {
            let reusable = old_rows.iter_mut().find(|row| {
                row.name.eq_ignore_ascii_case(&param.name)
                    && row.data_type == param.data_type
                    && row.is_output == param.is_output
                    && row.editable == param.editable()
            });
            if let Some(old) = reusable {
                rows.push(crate::procedure_mode::ProcParamRow {
                    name: param.name.clone(),
                    data_type: param.data_type.clone(),
                    base_type: param.base_type.clone(),
                    family: param.family,
                    is_output: param.is_output,
                    editable: param.editable(),
                    has_default: param.has_default(),
                    null_checked: old.null_checked,
                    bit_checked: old.bit_checked,
                    input: old.input.take(),
                    last_value: std::mem::take(&mut old.last_value),
                    output_value: old.output_value.take(),
                    validation_error: old.validation_error.take(),
                });
                continue;
            }
            // New (or re-typed) parameter: pre-fill simple literal defaults.
            let mut null_checked = false;
            let mut bit_checked = false;
            let mut initial = String::new();
            if let Some(expr) = &param.default_expr {
                if crate::procedure_mode::default_is_null(expr) {
                    null_checked = true;
                } else if let Some(value) = crate::procedure_mode::default_prefill(expr) {
                    if param.family == sqlly_intel::query::procedure::TypeFamily::Bit {
                        bit_checked = matches!(value.as_str(), "1" | "true" | "True");
                    } else {
                        initial = value;
                    }
                }
            }
            let input = if param.editable()
                && param.family != sqlly_intel::query::procedure::TypeFamily::Bit
            {
                let placeholder: &'static str = match param.family {
                    sqlly_intel::query::procedure::TypeFamily::DateTime => {
                        if param.base_type == "DATE" {
                            "yyyy-mm-dd"
                        } else if param.base_type == "TIME" {
                            "hh:mm:ss"
                        } else {
                            "yyyy-mm-dd hh:mm:ss"
                        }
                    }
                    sqlly_intel::query::procedure::TypeFamily::Guid => {
                        "00000000-0000-0000-0000-000000000000"
                    }
                    sqlly_intel::query::procedure::TypeFamily::Binary => "0x…",
                    _ => "",
                };
                let state = cx.new(|cx| {
                    let mut state = InputState::new(window, cx);
                    if !placeholder.is_empty() {
                        state = state.placeholder(placeholder);
                    }
                    if !initial.is_empty() {
                        state = state.default_value(initial.clone());
                    }
                    state
                });
                let name = param.name.clone();
                cx.subscribe_in(
                    &state,
                    window,
                    move |this: &mut Self, input, event: &InputEvent, _window, cx| {
                        if let InputEvent::Change = event {
                            let value = input.read(cx).value().to_string();
                            let active = this.active_tab;
                            if let Some(tab) = this.tabs.get_mut(active)
                                && let Some(row) = tab
                                    .proc_mode
                                    .rows
                                    .iter_mut()
                                    .find(|r| r.name.eq_ignore_ascii_case(&name))
                            {
                                let dialect = tab.proc_mode.dialect;
                                row.validation_error =
                                    crate::procedure_mode::live_validate_for(row, &value, dialect);
                                row.last_value = value;
                            }
                        }
                    },
                )
                .detach();
                Some(state)
            } else {
                None
            };
            rows.push(crate::procedure_mode::ProcParamRow {
                name: param.name.clone(),
                data_type: param.data_type.clone(),
                base_type: param.base_type.clone(),
                family: param.family,
                is_output: param.is_output,
                editable: param.editable(),
                has_default: param.has_default(),
                null_checked,
                bit_checked,
                input,
                last_value: initial,
                output_value: None,
                validation_error: None,
            });
        }
        self.tabs.get_mut(tab_idx).unwrap().proc_mode.rows = rows;
    }

    /// The parameter strip: a flat band between the editor and the results,
    /// rendered only while Procedure mode is on. Hairline-separated, dense
    /// mono columns: name, type, NULL checkbox, typed value input, and a
    /// trailing output cell for OUTPUT params.
    pub(super) fn render_proc_strip(
        &mut self,
        tab_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = Theme::scaled;
        if !self
            .tabs
            .get(tab_idx)
            .is_some_and(|tab| tab.proc_mode.enabled)
        {
            return None;
        }
        self.sync_proc_rows(tab_idx, window, cx);
        let tab = self.tabs.get(tab_idx)?;
        let running = tab.proc_mode.running || tab.active_query_id.is_some();
        let rollback = tab.proc_mode.rollback;
        let return_code = tab.proc_mode.return_code.clone();

        let (name_text, verb_text, note): (String, Option<&'static str>, Option<String>) =
            match &tab.proc_mode.parsed {
                Some(Ok(header)) => (
                    header.qualified_name(),
                    Some(header.verb.label()),
                    if header.params.is_empty() {
                        Some(crate::localization::text("shell.procedure.no_parameters"))
                    } else {
                        None
                    },
                ),
                Some(Err(err)) => (
                    String::new(),
                    None,
                    Some(crate::localization::format(
                        "shell.procedure.header_invalid",
                        &[("error", err)],
                    )),
                ),
                None => (
                    String::new(),
                    None,
                    Some(crate::localization::text("shell.procedure.not_procedure")),
                ),
            };
        let has_header = matches!(tab.proc_mode.parsed, Some(Ok(_)));

        // Header: PROC glyph + qualified name + verb badge left; return code
        // and the Rollback checkbox right.
        let mut header_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(12.0)))
            .py(px(s(3.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.75)))
                    .text_color(Theme::text_dim())
                    .child("PROC"),
            );
        if !name_text.is_empty() {
            header_row = header_row.child(
                div()
                    .max_w(px(s(420.0)))
                    .truncate()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(name_text),
            );
        }
        if let Some(verb) = verb_text {
            header_row = header_row.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.75)))
                    .text_color(Theme::text_dim())
                    .child(verb),
            );
        }
        header_row = header_row.child(div().flex_1());
        if let Some(code) = return_code {
            header_row = header_row.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "shell.procedure.returned",
                        &[("code", &code)],
                    )),
            );
        }
        if has_header {
            header_row = header_row.child(
                Checkbox::new("proc-rollback")
                    .label(crate::localization::text("shell.procedure.rollback"))
                    .checked(rollback)
                    .disabled(running)
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        let active = this.active_tab;
                        if let Some(tab) = this.tabs.get_mut(active) {
                            tab.proc_mode.rollback = *checked;
                        }
                        cx.notify();
                    })),
            );
        }

        let mut body = div().flex().flex_col();
        if let Some(note) = note {
            body = body.child(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(note),
            );
        } else {
            let mut rows_el = div().flex().flex_col();
            for row_index in 0..tab.proc_mode.rows.len() {
                rows_el =
                    rows_el.child(self.render_proc_param_row(tab_idx, row_index, running, cx));
            }
            // Roughly four rows visible; longer parameter lists scroll.
            body = body.child(
                div()
                    .id("proc-strip-rows")
                    .max_h(px(s(4.0 * 34.0)))
                    .overflow_y_scroll()
                    .child(rows_el),
            );
        }

        Some(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .bg(Theme::bg())
                .border_b_1()
                .border_color(Theme::border())
                .child(header_row)
                .child(body)
                .into_any_element(),
        )
    }

    /// One parameter row: name, declared type, NULL checkbox, the typed value
    /// input (a Checkbox for `bit`, a validated text Input for everything
    /// editable, a quiet note for unsupported types), and the trailing output
    /// cell for OUTPUT params.
    fn render_proc_param_row(
        &mut self,
        tab_idx: usize,
        row_index: usize,
        running: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let Some(tab) = self.tabs.get(tab_idx) else {
            return div().into_any_element();
        };
        let Some(row) = tab.proc_mode.rows.get(row_index) else {
            return div().into_any_element();
        };
        let name = row.name.clone();
        let data_type = row.data_type.clone();
        let is_output = row.is_output;
        let editable = row.editable;
        let has_default = row.has_default;
        let null_checked = row.null_checked;
        let bit_checked = row.bit_checked;
        let family = row.family;
        let output_value = row.output_value.clone();
        let validation_error = row.validation_error.clone();
        let input = row.input.clone();

        let mut line = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(12.0)))
            .py(px(s(3.0)));
        line = line.child(
            div()
                .w(px(s(160.0)))
                .flex_shrink_0()
                .truncate()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text())
                .child(format!("@{name}")),
        );
        line = line.child(
            div()
                .w(px(s(120.0)))
                .flex_shrink_0()
                .truncate()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(data_type),
        );
        line = line.child(
            Checkbox::new(("proc-null", row_index))
                .label("NULL")
                .checked(null_checked)
                .disabled(running || !editable)
                .on_click(cx.listener(move |this, checked: &bool, _window, cx| {
                    let active = this.active_tab;
                    if let Some(tab) = this.tabs.get_mut(active)
                        && let Some(row) = tab.proc_mode.rows.get_mut(row_index)
                    {
                        row.null_checked = *checked;
                        row.validation_error = None;
                        let restore = row.last_value.clone();
                        if !*checked && let Some(input) = &row.input {
                            input.update(cx, |state, cx| {
                                state.set_value(restore, _window, cx);
                            });
                        }
                    }
                    cx.notify();
                })),
        );

        // Value editor: bit gets a checkbox, unsupported types a quiet note,
        // everything else the validated text input.
        if !editable {
            let note = if family == sqlly_intel::query::procedure::TypeFamily::Unsupported {
                if has_default {
                    crate::localization::text("shell.procedure.unsupported_with_default")
                } else {
                    crate::localization::text("shell.procedure.unsupported_without_default")
                }
            } else {
                crate::localization::text("shell.procedure.read_only_table_parameter")
            };
            line = line.child(
                div()
                    .flex_1()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(note),
            );
        } else if family == sqlly_intel::query::procedure::TypeFamily::Bit {
            line = line.child(
                div().flex_1().child(
                    Checkbox::new(("proc-bit", row_index))
                        .checked(bit_checked)
                        .disabled(running || null_checked)
                        .on_click(cx.listener(move |this, checked: &bool, _window, cx| {
                            let active = this.active_tab;
                            if let Some(tab) = this.tabs.get_mut(active)
                                && let Some(row) = tab.proc_mode.rows.get_mut(row_index)
                            {
                                row.bit_checked = *checked;
                                row.validation_error = None;
                            }
                            cx.notify();
                        })),
                ),
            );
        } else if let Some(input) = input {
            line = line.child(
                div()
                    .flex_1()
                    .child(Input::new(&input).disabled(running || null_checked)),
            );
        } else {
            line = line.child(div().flex_1());
        }

        if is_output {
            line = line.child(
                div()
                    .w(px(s(160.0)))
                    .flex_shrink_0()
                    .truncate()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(output_value.unwrap_or_else(|| "···".to_string())),
            );
        }

        let mut cell = div().flex().flex_col();
        cell = cell.child(line);
        if let Some(error) = validation_error {
            cell = cell.child(
                div()
                    .px(px(s(12.0)))
                    .pb(px(s(2.0)))
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::error())
                    .child(error),
            );
        }
        cell.into_any_element()
    }
}

#[cfg(test)]
mod procedure_run_tests {
    use crate::procedure_mode::{ProcParamRow, ProcedureModeState, build_plan, run_proc_sequence};
    use async_trait::async_trait;
    use sqlly_core::{CellValue, SecretProvider, SecretRef};
    use sqlly_intel::query::procedure;
    use sqlly_intel::query::procedure::TypeFamily;
    use sqlly_services::QueryEvent;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    struct FakeQueries {
        executed: Mutex<Vec<String>>,
        probe_exists: bool,
    }

    impl FakeQueries {
        fn new(probe_exists: bool) -> Self {
            Self {
                executed: Mutex::new(Vec::new()),
                probe_exists,
            }
        }

        fn executed_sql(&self) -> Vec<String> {
            self.executed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl sqlly_services::QueryExecutor for FakeQueries {
        async fn execute(
            &self,
            _profile: &sqlly_core::ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<sqlly_services::QueryHandle> {
            self.executed.lock().unwrap().push(sql.to_string());
            let (tx, rx) = mpsc::channel(16);
            if sql.contains("OBJECT_ID") || sql.contains("to_regprocedure") {
                let rows = if self.probe_exists {
                    vec![vec![CellValue::I64(1)]]
                } else {
                    vec![]
                };
                if !rows.is_empty() {
                    tx.send(QueryEvent::Rows { index: 0, rows }).await.ok();
                }
                tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
            } else {
                tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
            }
            Ok(sqlly_services::QueryHandle {
                task_id: "proc-test".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
            Ok(())
        }
    }

    fn profile() -> sqlly_core::ConnectionProfile {
        sqlly_core::ConnectionProfile::sql_login(
            "proc-test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        )
    }

    fn param_row(name: &str, value: &str) -> ProcParamRow {
        ProcParamRow {
            name: name.to_string(),
            data_type: "int".to_string(),
            base_type: "INT".to_string(),
            family: TypeFamily::Integer,
            is_output: false,
            editable: true,
            has_default: false,
            null_checked: false,
            bit_checked: false,
            input: None,
            last_value: value.to_string(),
            output_value: None,
            validation_error: None,
        }
    }

    fn sample_plan(rollback: bool) -> crate::procedure_mode::ProcRunPlan {
        let script = "CREATE PROC dbo.ProcRunTest @in int AS SELECT 1";
        let header = procedure::parse_proc_header(script).unwrap();
        build_plan(script, &header, &[param_row("in", "3")], rollback).unwrap()
    }

    #[tokio::test]
    async fn deploy_invoke_runs_probe_then_deploy_then_invoke() {
        let queries = Arc::new(FakeQueries::new(false));
        let plan = sample_plan(false);
        let (tx, mut rx) = mpsc::channel(8);
        let profile = profile();
        run_proc_sequence(queries.clone(), profile, plan, tx).await;
        while rx.try_recv().is_ok() {}
        let sql = queries.executed_sql();
        assert_eq!(sql.len(), 3, "{sql:?}");
        assert!(sql[0].contains("OBJECT_ID"), "probe: {}", sql[0]);
        assert_eq!(sql[1], "CREATE PROC dbo.ProcRunTest @in int AS SELECT 1");
        assert!(sql[2].contains("EXEC"), "invoke: {}", sql[2]);
    }

    #[tokio::test]
    async fn rollback_runs_single_composite_batch() {
        let queries = Arc::new(FakeQueries::new(false));
        let plan = sample_plan(true);
        let (tx, mut rx) = mpsc::channel(8);
        run_proc_sequence(queries.clone(), profile(), plan, tx).await;
        while rx.try_recv().is_ok() {}
        let sql = queries.executed_sql();
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert!(sql[0].contains("OBJECT_ID"), "probe: {}", sql[0]);
        assert!(sql[1].contains("BEGIN TRAN"), "{}", sql[1]);
        assert!(sql[1].contains("ROLLBACK"), "{}", sql[1]);
    }

    #[test]
    fn cancel_clears_proc_mode_running() {
        use crate::app::execute::take_proc_mode_running;
        let mut state = ProcedureModeState {
            running: true,
            ..Default::default()
        };
        assert!(take_proc_mode_running(&mut state.running));
        assert!(!state.running);
    }
}
