//! AI panes: on-device runtime probe, FIM, cloud providers, playground.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ============================================================
    // AI > On device
    // ============================================================

    /// Every control this pane used to show was wired to `noop_checkbox` with
    /// a hardcoded value, so for a while it honestly rendered as coming-soon.
    /// The query-summary settings are real now — `AiSummaryPreferences` backs
    /// them and the app reads them — and the runtime section below is
    /// deliberately read-only: the endpoint is not configurable, so the pane
    /// reports it and probes it rather than pretending to let you set it.
    pub(super) fn render_ai_on_device(&self, cx: &mut Context<Self>) -> AnyElement {
        let ai = self.store.ai_summary();
        let checking = matches!(self.ai_runtime_probe, AiRuntimeProbe::Checking);

        let mut children = vec![
            self.heading(&crate::localization::text("settings.ai.heading.device")),
            self.intro(&crate::localization::text("settings.ai.intro.on_device")),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.query_summaries",
            )),
            self.checkbox(
                "ai_summary_enabled",
                &crate::localization::text("settings.ai.checkbox.show_ai_query_summary_bar"),
                ai.enabled,
                cx,
                |this, cx| {
                    this.set_ai_summary_field(cx, |p| p.enabled = !p.enabled);
                },
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.summary_bar_help",
            )),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.model"),
                div()
                    .min_w(px(Theme::scaled(280.)))
                    .child(self.prefs_input("ai_summary_model"))
                    .into_any_element(),
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.summary_model_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.agent_mode",
            )),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.max_agent_turns"),
                div()
                    .min_w(px(Theme::scaled(120.)))
                    .child(self.prefs_input("agent_max_turns"))
                    .into_any_element(),
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.drawer_tool_round_limit",
            )),
            self.hint(&crate::localization::text(
                "settings.ai.hint.agent_read_only_safeguards",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.local_ai_runtime",
            )),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.endpoint"),
                self.read_only_value(crate::query_ai_summary::effective_base_url("")),
            ),
            div()
                .flex()
                .flex_row()
                .child(self.quiet_button(
                    "ai_runtime_check",
                    &if checking {
                        crate::localization::text("settings.ai.static.checking")
                    } else {
                        crate::localization::text("settings.ai.computed.check")
                    },
                    cx,
                    |this, _window, cx| {
                        this.handle_check_ai_runtime(cx);
                    },
                ))
                .into_any_element(),
        ];

        match &self.ai_runtime_probe {
            AiRuntimeProbe::Idle => {
                children.push(self.hint(&crate::localization::text(
                    "settings.ai.hint.runtime_check_help",
                )));
            }
            AiRuntimeProbe::Checking => {
                children.push(self.hint(&crate::localization::text(
                    "settings.ai.hint.contacting_local_ai_runtime",
                )));
            }
            AiRuntimeProbe::Reachable(models) => {
                if models.is_empty() {
                    children.push(self.hint(&crate::localization::text(
                        "settings.ai.hint.runtime_no_models",
                    )));
                } else {
                    let key = if models.len() == 1 {
                        "settings.ai.runtime_models.single"
                    } else {
                        "settings.ai.runtime_models.plural"
                    };
                    children.push(self.hint(&crate::localization::format(
                        key,
                        &[
                            ("count", &models.len().to_string()),
                            ("models", &models.join(", ")),
                        ],
                    )));
                }
            }
            AiRuntimeProbe::Unreachable(error) => {
                children.push(self.warning(&crate::localization::format(
                    "settings.ai.message.unreachable_error",
                    &[("error", &(error).to_string())],
                )));
                children.push(self.hint(&crate::localization::text(
                    "settings.ai.hint.install_ollama_help",
                )));
            }
        }

        self.pane_container(children)
    }

    /// Spawn a background probe of the local AI runtime (`/api/tags`) and
    /// store the outcome in `ai_runtime_probe`. Mirrors the
    /// [`Self::handle_fetch_cloud_models`] bridge: the HTTP work runs on the
    /// tokio runtime, the result crosses back over a oneshot channel, and a
    /// generation token discards completions that lost a race.
    pub(super) fn handle_check_ai_runtime(&mut self, cx: &mut Context<Self>) {
        if matches!(self.ai_runtime_probe, AiRuntimeProbe::Checking) {
            return;
        }
        self.ai_runtime_probe_generation += 1;
        let generation = self.ai_runtime_probe_generation;
        self.ai_runtime_probe = AiRuntimeProbe::Checking;
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<String>, String>>();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = crate::query_ai_summary::list_installed_models("")
                        .await
                        .map_err(|e| e.to_string());
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
                    .update(&mut cx, |this, cx| {
                        if this.ai_runtime_probe_generation != generation {
                            return;
                        }
                        this.ai_runtime_probe = match result {
                            Ok(models) => {
                                this.ai_playground.update(cx, |playground, cx| {
                                    playground.set_ollama_models(models.clone(), cx)
                                });
                                AiRuntimeProbe::Reachable(models)
                            }
                            Err(e) => AiRuntimeProbe::Unreachable(e),
                        };
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    // ============================================================
    // AI > Autocomplete (FIM)
    // ============================================================

    /// Unlike the other AI panes, this one has a real consumer:
    /// `FimCompletionService` reads every field below, and `schedule_fim_request`
    /// gates on `enabled`. The controls were still hardcoded literals — and the
    /// literals disagreed with the struct's own defaults (the pane claimed 2048
    /// max context tokens against a default of 4096, temperature 0.2 against
    /// 0.1), so the numbers on screen described no state that ever existed.
    pub(super) fn render_ai_fim(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let f = self.store.fim();
        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.ai.heading.autocomplete_fim",
            )),
            self.intro(&crate::localization::text("settings.ai.intro.autocomplete")),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.enable",
            )),
            self.fim_check(
                "fim_enabled",
                &crate::localization::text("settings.ai.fim_check.enable_autocomplete"),
                f.enabled,
                |p| p.enabled = !p.enabled,
                cx,
            ),
            self.fim_check(
                "fim_new_queries",
                &crate::localization::text("settings.ai.fim_check.enable_new_queries_by_default"),
                f.enable_for_new_queries,
                |p| p.enable_for_new_queries = !p.enable_for_new_queries,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.model",
            )),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.model_2"),
                self.enum_select(
                    "fim_model",
                    f.model,
                    FimAiModel::all().to_vec(),
                    |v| v.display_name().to_string(),
                    |this, v, cx| this.set_fim_field(cx, |p| p.model = v),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.fim_model_size_tradeoff",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.prompt_format",
            )),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.format"),
                self.enum_select(
                    "fim_format",
                    f.fim_format,
                    FimPromptFormat::all().to_vec(),
                    |v| v.display_name().to_string(),
                    |this, v, cx| this.set_fim_field(cx, |p| p.fim_format = v),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.format_auto_custom",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.trigger",
            )),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.trigger"),
                self.enum_select(
                    "fim_trigger",
                    f.trigger_kind,
                    vec![FimTriggerKind::Automatic, FimTriggerKind::Manual],
                    |v| v.display_name().to_string(),
                    |this, v, cx| this.set_fim_field(cx, |p| p.trigger_kind = v),
                    window,
                    cx,
                ),
            ),
            self.input_row(
                &crate::localization::text("settings.ai.input_row.generation_delay_s"),
                "fim_delay",
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.generation_delay_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.shortcuts",
            )),
            // The trigger/accept keys are keybindings, not FIM settings — they
            // live in the keymap, and no field here backs them. Two dropdowns
            // showing "⌃ Space" and "↩" wired to nothing implied otherwise.
            self.hint(&crate::localization::text(
                "settings.ai.hint.shortcuts_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.generation",
            )),
            self.input_row(
                &crate::localization::text("settings.ai.input_row.max_context_tokens"),
                "fim_max_ctx",
            ),
            self.input_row(
                &crate::localization::text("settings.ai.input_row.suffix_budget_0_1"),
                "fim_suffix",
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.suffix_budget_help",
            )),
            self.input_row(
                &crate::localization::text("settings.ai.input_row.temperature"),
                "fim_temperature",
            ),
            self.input_row(
                &crate::localization::text("settings.ai.input_row.max_new_tokens"),
                "fim_max_new",
            ),
            self.fim_check(
                "fim_stop_nl",
                &crate::localization::text("settings.ai.fim_check.stop_newline"),
                f.stop_on_newline,
                |p| p.stop_on_newline = !p.stop_on_newline,
                cx,
            ),
            self.fim_check(
                "fim_trim_ws",
                &crate::localization::text("settings.ai.fim_check.trim_leading_whitespace"),
                f.trim_leading_whitespace,
                |p| p.trim_leading_whitespace = !p.trim_leading_whitespace,
                cx,
            ),
        ])
    }

    // ============================================================
    // AI > Cloud
    // ============================================================

    pub(super) fn render_ai_cloud(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::cloud_ai::{ProviderKind, ReasoningLevel, egress_label};

        let mut children = vec![
            self.heading(&crate::localization::text("settings.ai.heading.providers")),
            self.intro(&crate::localization::text("settings.ai.intro.providers")),
        ];

        let selected = self.cloud_ai.selected.clone();
        let mut list = div().flex().flex_col();
        for profile in &self.cloud_ai.profiles.profiles {
            let profile_id = profile.id.clone();
            let is_selected = selected.as_deref() == Some(profile_id.as_str());
            list = list.child(
                div()
                    .id(format!("ai_profile_row_{profile_id}"))
                    .h(px(Theme::scaled(26.)))
                    .flex()
                    .items_center()
                    .px(px(Theme::scaled(8.)))
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(Theme::border())
                    .bg(if is_selected {
                        Theme::selection()
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if is_selected {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(self.match_text(&format!(
                        "{} · {}",
                        profile.name,
                        profile.kind.display_name()
                    )))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener({
                            let profile_id = profile_id.clone();
                            move |this, _, window, cx| {
                                this.select_cloud_profile(&profile_id, window, cx);
                            }
                        }),
                    ),
            );
            list = list.child(self.hint(&egress_label(profile)));
        }
        children.push(list.into_any_element());

        children.push(
            div()
                .flex()
                .flex_row()
                .gap(px(Theme::scaled(8.)))
                .child(self.quiet_button(
                    "ai_profile_add",
                    &crate::localization::text("settings.ai.quiet_button.add_profile"),
                    cx,
                    {
                        move |this, _window, cx| {
                            this.cloud_ai.adding_kind = true;
                            cx.notify();
                        }
                    },
                ))
                .into_any_element(),
        );

        if self.cloud_ai.adding_kind {
            children.push(self.hint(&crate::localization::text(
                "settings.ai.hint.choose_new_profile_kind",
            )));
            children.push(self.enum_select_in(
                "ai_profile_add_kind",
                ProviderKind::OpenAiCompatible,
                ProviderKind::all().to_vec(),
                |kind| kind.display_name().to_string(),
                |this, kind, window, cx| {
                    this.add_cloud_profile(kind, window, cx);
                },
                window,
                cx,
            ));
        }

        if let Some(profile_id) = selected.clone()
            && let Some(profile) = self
                .cloud_ai
                .profiles
                .profiles
                .iter()
                .find(|profile| profile.id == profile_id)
        {
            let saved = self
                .cloud_ai
                .key_saved
                .get(&profile_id)
                .copied()
                .unwrap_or(false);
            let typed = self
                .cloud_ai
                .key_input
                .get(&profile_id)
                .cloned()
                .unwrap_or_default();

            children.push(self.section_header(&crate::localization::text(
                "settings.ai.section_header.profile_details",
            )));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.name"),
                self.prefs_input("ai_profile_name"),
            ));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.kind"),
                self.read_only_value(profile.kind.display_name()),
            ));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.base_url"),
                self.prefs_input("ai_profile_base_url"),
            ));
            children.push(self.hint(&crate::localization::text("settings.ai.hint.base_url_help")));

            if saved && typed.is_empty() {
                children.push(self.labeled_row(
                    &crate::localization::text("settings.ai.labeled_row.api_key"),
                    self.read_only_value(&crate::localization::text(
                        "settings.ai.computed.saved_to_your_keychain",
                    )),
                ));
                children.push(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(Theme::scaled(8.)))
                        .child(self.quiet_button(
                            "ai_profile_replace_key",
                            &crate::localization::text("settings.ai.quiet_button.replace_key"),
                            cx,
                            {
                                let profile_id = profile_id.clone();
                                move |this, window, cx| {
                                    this.cloud_ai
                                        .key_input
                                        .insert(profile_id.clone(), String::new());
                                    this.set_input_value("ai_profile_key", "", window, cx);
                                    cx.notify();
                                }
                            },
                        ))
                        .child(self.quiet_button(
                            "ai_profile_remove_key",
                            &crate::localization::text("settings.ai.quiet_button.remove_key"),
                            cx,
                            {
                                let profile_id = profile_id.clone();
                                move |this, window, cx| {
                                    this.handle_remove_cloud_key(&profile_id, window, cx);
                                }
                            },
                        ))
                        .into_any_element(),
                );
            } else {
                children.push(
                    self.labeled_row(
                        &crate::localization::text("settings.ai.labeled_row.api_key_2"),
                        div()
                            .min_w(px(Theme::scaled(280.)))
                            .child(self.prefs_input("ai_profile_key"))
                            .into_any_element(),
                    ),
                );
                let can_save = !typed.trim().is_empty();
                let mut row = div().flex().flex_row().gap(px(Theme::scaled(8.)));
                if can_save {
                    row = row.child(self.quiet_button(
                        "ai_profile_save_key",
                        &crate::localization::text("settings.ai.quiet_button.save_key"),
                        cx,
                        {
                            let profile_id = profile_id.clone();
                            move |this, window, cx| {
                                this.handle_save_cloud_key(&profile_id, window, cx);
                            }
                        },
                    ));
                }
                if saved {
                    row = row.child(self.quiet_button(
                        "ai_profile_cancel_key",
                        &crate::localization::text("settings.ai.quiet_button.cancel"),
                        cx,
                        {
                            let profile_id = profile_id.clone();
                            move |this, window, cx| {
                                this.cloud_ai.key_input.remove(&profile_id);
                                this.set_input_value("ai_profile_key", "", window, cx);
                                cx.notify();
                            }
                        },
                    ));
                }
                children.push(row.into_any_element());
                if !saved {
                    children.push(self.hint(&crate::localization::text(
                        "settings.ai.hint.cloud_key_not_saved",
                    )));
                }
            }

            let model_options: Vec<DropdownOption> = profile
                .available_model_ids
                .iter()
                .map(|model| {
                    let selected = profile.model == *model;
                    let model = model.clone();
                    let profile_id = profile_id.clone();
                    (
                        model.clone(),
                        selected,
                        std::rc::Rc::new(
                            move |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                                if let Some(profile) = this
                                    .cloud_ai
                                    .profiles
                                    .profiles
                                    .iter_mut()
                                    .find(|p| p.id == profile_id)
                                {
                                    profile.model = model.clone();
                                    let _ =
                                        crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                                }
                                cx.notify();
                            },
                        ) as DropdownHandler,
                    )
                })
                .collect();
            children.push(
                self.labeled_row(
                    &crate::localization::text("settings.ai.labeled_row.model_3"),
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(Theme::scaled(6.)))
                        .child(if model_options.is_empty() {
                            div().into_any_element()
                        } else {
                            self.select_dropdown(
                                "ai_profile_model_select",
                                "",
                                model_options,
                                window,
                                cx,
                            )
                        })
                        .child(self.prefs_input("ai_profile_model"))
                        .into_any_element(),
                ),
            );

            let fetching = self.cloud_ai.fetching;
            let testing = self.cloud_ai.testing;
            let supports_model_list = profile.kind.supports_model_list();
            children.push(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.)))
                    .child(self.quiet_button(
                        "ai_profile_fetch",
                        &if fetching {
                            crate::localization::text("settings.ai.computed.fetching")
                        } else {
                            crate::localization::text("settings.ai.computed.fetch_models")
                        },
                        cx,
                        {
                            let profile_id = profile_id.clone();
                            move |this, _window, cx| {
                                if !this.cloud_ai.fetching && supports_model_list {
                                    this.handle_fetch_cloud_models(&profile_id, cx);
                                }
                            }
                        },
                    ))
                    .child(self.quiet_button(
                        "ai_profile_test_latency",
                        &if testing {
                            crate::localization::text("settings.ai.computed.testing")
                        } else {
                            crate::localization::text("settings.ai.computed.test_latency")
                        },
                        cx,
                        {
                            let profile_id = profile_id.clone();
                            move |this, _window, cx| {
                                if !this.cloud_ai.testing {
                                    this.handle_test_cloud_latency(&profile_id, cx);
                                }
                            }
                        },
                    ))
                    .into_any_element(),
            );
            if let Some(duration) = self.cloud_ai.latency.get(&profile_id) {
                let host = crate::cloud_ai::host_from_base_url(&profile.base_url);
                children.push(self.hint(&crate::localization::format(
                    "settings.ai.message.ms_host",
                    &[
                        ("value", &(duration.as_millis()).to_string()),
                        ("host", &(host).to_string()),
                    ],
                )));
            }

            children.push(self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.reasoning"),
                self.enum_select(
                    "ai_profile_reasoning",
                    profile.reasoning,
                    vec![
                        ReasoningLevel::Off,
                        ReasoningLevel::Low,
                        ReasoningLevel::Medium,
                        ReasoningLevel::High,
                    ],
                    |level| match level {
                        ReasoningLevel::Off => crate::localization::text("settings.ai.option.off"),
                        ReasoningLevel::Low => crate::localization::text("settings.ai.option.low"),
                        ReasoningLevel::Medium => {
                            crate::localization::text("settings.ai.option.medium")
                        }
                        ReasoningLevel::High => {
                            crate::localization::text("settings.ai.option.high")
                        }
                    },
                    {
                        let profile_id = profile_id.clone();
                        move |this, level, cx| {
                            if let Some(profile) = this
                                .cloud_ai
                                .profiles
                                .profiles
                                .iter_mut()
                                .find(|p| p.id == profile_id)
                            {
                                profile.reasoning = level;
                                let _ = crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                            }
                            cx.notify();
                        }
                    },
                    window,
                    cx,
                ),
            ));

            if matches!(
                profile.kind,
                ProviderKind::Gemini | ProviderKind::AnthropicCompatible
            ) {
                children.push(self.labeled_row(
                    &crate::localization::text("settings.ai.labeled_row.thinking_budget_tokens"),
                    self.prefs_input("ai_profile_thinking_budget"),
                ));
            }

            if matches!(
                profile.kind,
                ProviderKind::OpenAiCompatible | ProviderKind::CheaperInference
            ) {
                children.push(self.checkbox(
                    "ai_profile_send_effort",
                    &crate::localization::text("settings.ai.checkbox.send_reasoning_effort"),
                    profile.send_reasoning_effort,
                    cx,
                    {
                        let profile_id = profile_id.clone();
                        move |this, cx| {
                            if let Some(profile) = this
                                .cloud_ai
                                .profiles
                                .profiles
                                .iter_mut()
                                .find(|p| p.id == profile_id)
                            {
                                profile.send_reasoning_effort = !profile.send_reasoning_effort;
                                let _ = crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                            }
                            cx.notify();
                        }
                    },
                ));
            }

            if let Some(note) = profile.kind.reasoning_note() {
                children.push(self.hint(&note));
            }

            children.push(
                self.quiet_button(
                    "ai_profile_remove",
                    &crate::localization::text("settings.ai.quiet_button.remove_profile"),
                    cx,
                    {
                        let profile_id = profile_id.clone();
                        move |this, window, cx| {
                            this.remove_cloud_profile(&profile_id, window, cx);
                        }
                    },
                )
                .into_any_element(),
            );
        }

        children.push(self.section_header(&crate::localization::text(
            "settings.ai.section_header.app_ai_backend",
        )));
        children.push(self.hint(&crate::localization::text(
            "settings.ai.hint.app_backend_help",
        )));

        let active = self.cloud_ai.profiles.active_profile_id.clone();
        let mut backend_options: Vec<DropdownOption> = vec![(
            crate::localization::text("settings.ai.computed.local_runtime").to_string(),
            active.is_none(),
            std::rc::Rc::new(
                |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                    this.set_app_ai_profile(None, cx);
                },
            ),
        )];
        for profile in &self.cloud_ai.profiles.profiles {
            let profile_id = profile.id.clone();
            let label = profile.name.clone();
            let selected = active.as_deref() == Some(profile_id.as_str());
            backend_options.push((
                label,
                selected,
                std::rc::Rc::new(
                    move |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                        this.set_app_ai_profile(Some(profile_id.clone()), cx);
                    },
                ),
            ));
        }
        children.push(self.select_dropdown(
            "ai_app_backend_select",
            "",
            backend_options,
            window,
            cx,
        ));

        if let Some(active_id) = active.as_ref()
            && let Some(profile) = self
                .cloud_ai
                .profiles
                .profiles
                .iter()
                .find(|profile| &profile.id == active_id)
        {
            children.push(self.hint(&egress_label(profile)));
            if profile.model.trim().is_empty() {
                children.push(self.warning(&crate::localization::text(
                    "settings.ai.warning.active_profile_no_model",
                )));
            }
        }

        if let Some(ref status) = self.cloud_ai.status {
            children.push(self.hint(status));
        }

        self.pane_container(children)
    }

    // ============================================================
    // AI > Decision model
    // ============================================================

    /// Settings for the optional decision-model provider (Jev). Opt-in and
    /// default off; the API key follows the same keychain discipline as the
    /// Cloud pane. `allow_row_data` is a separate switch because result rows
    /// are a different privacy class than schema names and SQL text.
    pub(super) fn render_ai_decision_model(&self, cx: &mut Context<Self>) -> AnyElement {
        let config = &self.decision_ai.config;
        let host = crate::cloud_ai::host_from_base_url(&config.base_url);
        let testing = self.decision_ai.testing;
        let saved = self.decision_ai.key_saved;
        let typed = self.decision_ai.key_input.clone();

        let mut children = vec![
            self.heading(&crate::localization::text(
                "settings.ai.heading.decision_model",
            )),
            self.intro(&crate::localization::text(
                "settings.ai.intro.decision_model",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.enable_2",
            )),
            self.checkbox(
                "decision_ai_enabled",
                &crate::localization::text("settings.ai.checkbox.enable_decision_model"),
                config.enabled,
                cx,
                |this, cx| {
                    this.decision_ai.config.enabled = !this.decision_ai.config.enabled;
                    let _ = crate::decision_ai::save_decision_ai_config(&this.decision_ai.config);
                    cx.notify();
                },
            ),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.endpoint",
            )),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.base_url_2"),
                self.prefs_input("decision_ai_base_url"),
            ),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.model_4"),
                self.prefs_input("decision_ai_model"),
            ),
            self.hint(&crate::localization::text(
                "settings.ai.hint.pinned_model_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.ai.section_header.api_key",
            )),
        ];

        if saved && typed.is_empty() {
            children.push(self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.api_key_3"),
                self.read_only_value(&crate::localization::text(
                    "settings.ai.computed.saved_to_your_keychain_2",
                )),
            ));
            children.push(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.)))
                    .child(self.quiet_button(
                        "decision_ai_replace_key",
                        &crate::localization::text("settings.ai.quiet_button.replace_key_2"),
                        cx,
                        |this, window, cx| {
                            this.decision_ai.key_input = String::new();
                            this.set_input_value("decision_ai_key", "", window, cx);
                            this.decision_ai.key_saved = false;
                            cx.notify();
                        },
                    ))
                    .child(self.quiet_button(
                        "decision_ai_remove_key",
                        &crate::localization::text("settings.ai.quiet_button.remove_key_2"),
                        cx,
                        |this, window, cx| {
                            this.handle_remove_decision_key(window, cx);
                        },
                    ))
                    .into_any_element(),
            );
        } else {
            children.push(
                self.labeled_row(
                    &crate::localization::text("settings.ai.labeled_row.api_key_4"),
                    div()
                        .min_w(px(Theme::scaled(280.)))
                        .child(self.prefs_input("decision_ai_key"))
                        .into_any_element(),
                ),
            );
            let mut row = div().flex().flex_row().gap(px(Theme::scaled(8.)));
            if !typed.trim().is_empty() {
                row = row.child(self.quiet_button(
                    "decision_ai_save_key",
                    &crate::localization::text("settings.ai.quiet_button.save_key_2"),
                    cx,
                    |this, window, cx| {
                        this.handle_save_decision_key(window, cx);
                    },
                ));
            }
            children.push(row.into_any_element());
            children.push(self.hint(&crate::localization::text("settings.ai.hint.key_not_saved")));
        }

        children.push(self.section_header(&crate::localization::text(
            "settings.ai.section_header.connection",
        )));
        children.push(
            self.quiet_button(
                "decision_ai_test",
                &if testing {
                    crate::localization::text("settings.ai.computed.testing_2")
                } else {
                    crate::localization::text("settings.ai.computed.test")
                },
                cx,
                |this, _window, cx| {
                    if !this.decision_ai.testing {
                        this.handle_test_decision_model(cx);
                    }
                },
            )
            .into_any_element(),
        );
        if let Some(ref status) = self.decision_ai.status {
            children.push(self.hint(status));
        }

        children.push(self.section_header(&crate::localization::text(
            "settings.ai.section_header.privacy",
        )));
        children.push(self.checkbox(
            "decision_ai_allow_row_data",
            &crate::localization::text("settings.ai.checkbox.allow_result_data_sent"),
            config.allow_row_data,
            cx,
            |this, cx| {
                this.decision_ai.config.allow_row_data = !this.decision_ai.config.allow_row_data;
                let _ = crate::decision_ai::save_decision_ai_config(&this.decision_ai.config);
                cx.notify();
            },
        ));
        children.push(self.hint(&crate::localization::format(
            "settings.ai.message.privacy_egress_note",
            &[("host", &(host).to_string())],
        )));

        self.pane_container(children)
    }

    // ---- Decision model pane actions ----

    pub(super) fn handle_save_decision_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.decision_ai.key_input.clone();
        if key.trim().is_empty() {
            self.decision_ai.status = Some(
                crate::localization::text("settings.ai.static.enter_an_api_key_first").to_string(),
            );
            cx.notify();
            return;
        }
        let account = self.decision_ai.config.api_key_ref.clone();
        match crate::cloud_ai::store_api_key_ref(&account, &key) {
            Ok(()) => {
                self.decision_ai.key_input = String::new();
                self.set_input_value("decision_ai_key", "", window, cx);
                self.decision_ai.key_saved = true;
                self.decision_ai.status =
                    Some(crate::localization::text("settings.ai.static.key_saved").to_string());
            }
            Err(e) => {
                self.decision_ai.key_saved = false;
                self.decision_ai.status = Some(crate::localization::format(
                    "settings.ai.message.could_not_save_key_keychain_e",
                    &[("e", &(e).to_string())],
                ));
            }
        }
        cx.notify();
    }

    pub(super) fn handle_remove_decision_key(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let account = self.decision_ai.config.api_key_ref.clone();
        match crate::cloud_ai::delete_api_key_ref(&account) {
            Ok(()) => {
                self.decision_ai.key_saved = false;
                self.decision_ai.key_input = String::new();
                self.set_input_value("decision_ai_key", "", window, cx);
                self.decision_ai.status =
                    Some(crate::localization::text("settings.ai.static.key_removed").to_string());
            }
            Err(e) => {
                self.decision_ai.status = Some(crate::localization::format(
                    "settings.ai.message.could_not_remove_key_e",
                    &[("e", &(e).to_string())],
                ));
            }
        }
        cx.notify();
    }

    /// Sends one yes/no question with the state `"ping"` and reports round-trip
    /// latency or the error.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_test_decision_model(&mut self, cx: &mut Context<Self>) {
        if self.decision_ai.testing {
            return;
        }
        let config = self.decision_ai.config.clone();
        let typed = self.decision_ai.key_input.clone();
        let api_key = if !typed.trim().is_empty() {
            typed
        } else {
            match crate::cloud_ai::load_api_key_ref(&config.api_key_ref) {
                Ok(Some(key)) if !key.is_empty() => key,
                Ok(_) => {
                    self.decision_ai.status = Some(
                        crate::localization::text("settings.ai.static.decision_test_needs_key")
                            .to_string(),
                    );
                    cx.notify();
                    return;
                }
                Err(e) => {
                    self.decision_ai.status = Some(crate::localization::format(
                        "settings.ai.message.could_not_read_key_e",
                        &[("e", &(e).to_string())],
                    ));
                    cx.notify();
                    return;
                }
            }
        };

        self.decision_ai.testing = true;
        self.decision_ai.test_generation += 1;
        let generation = self.decision_ai.test_generation;
        let host = crate::cloud_ai::host_from_base_url(&config.base_url);
        self.decision_ai.status = Some(crate::localization::format(
            "settings.ai.message.testing_decision_model",
            &[("host", &(host).to_string())],
        ));
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<std::time::Duration, String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = test_decision_model(&config, api_key).await;
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
                    .update(&mut cx, |this, cx| {
                        if this.decision_ai.test_generation != generation {
                            return;
                        }
                        this.decision_ai.testing = false;
                        this.decision_ai.status = Some(match result {
                            Ok(duration) => crate::localization::format(
                                "settings.ai.message.answered_ms",
                                &[("value", &(duration.as_millis()).to_string())],
                            ),
                            Err(e) => e,
                        });
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// The Jev client is native-only; the browser build has no decision
    /// provider to test.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn handle_test_decision_model(&mut self, cx: &mut Context<Self>) {
        self.decision_ai.status = Some(crate::browser_preview::unavailable(
            &crate::localization::text("settings.ai.computed.decision_model_unavailable"),
        ));
        cx.notify();
    }

    fn select_cloud_profile(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cloud_ai.selected = Some(profile_id.to_string());
        self.sync_profile_inputs(window, cx);
        cx.notify();
    }

    fn sync_profile_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile_id) = self.cloud_ai.selected.clone() else {
            return;
        };
        let Some(profile) = self
            .cloud_ai
            .profiles
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            return;
        };
        self.set_input_value("ai_profile_name", &profile.name, window, cx);
        self.set_input_value("ai_profile_base_url", &profile.base_url, window, cx);
        self.set_input_value("ai_profile_model", &profile.model, window, cx);
        self.set_input_value("ai_profile_key", "", window, cx);
        self.set_input_value(
            "ai_profile_thinking_budget",
            &profile
                .thinking_budget
                .map(|budget| budget.to_string())
                .unwrap_or_default(),
            window,
            cx,
        );
    }

    fn add_cloud_profile(
        &mut self,
        kind: crate::cloud_ai::ProviderKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile = crate::cloud_ai::ProviderProfile::new(kind);
        let profile_id = profile.id.clone();
        self.cloud_ai.key_saved.insert(profile_id.clone(), false);
        self.cloud_ai.profiles.profiles.push(profile);
        self.cloud_ai.adding_kind = false;
        if let Err(error) = crate::cloud_ai::save_ai_profiles(&self.cloud_ai.profiles) {
            self.cloud_ai.status = Some(crate::localization::format(
                "settings.ai.message.could_not_save_profiles_error",
                &[("error", &(error).to_string())],
            ));
        } else {
            self.select_cloud_profile(&profile_id, window, cx);
            self.cloud_ai.status =
                Some(crate::localization::text("settings.ai.static.profile_added").to_string());
        }
        self.ai_playground.update(cx, |playground, cx| {
            playground.refresh_cloud_profiles(&self.cloud_ai.profiles, cx);
        });
        cx.notify();
    }

    fn remove_cloud_profile(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self
            .cloud_ai
            .profiles
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            return;
        };
        if profile.api_key_ref.starts_with("ai-profile.") {
            let _ = crate::cloud_ai::delete_api_key_ref(&profile.api_key_ref);
        }
        self.cloud_ai
            .profiles
            .profiles
            .retain(|p| p.id != profile_id);
        if self.cloud_ai.profiles.active_profile_id.as_deref() == Some(profile_id) {
            self.cloud_ai.profiles.active_profile_id = None;
        }
        if self.cloud_ai.selected.as_deref() == Some(profile_id) {
            self.cloud_ai.selected = self
                .cloud_ai
                .profiles
                .profiles
                .first()
                .map(|profile| profile.id.clone());
            if self.cloud_ai.selected.is_some() {
                self.sync_profile_inputs(window, cx);
            }
        }
        self.cloud_ai.key_saved.remove(profile_id);
        self.cloud_ai.key_input.remove(profile_id);
        self.cloud_ai.latency.remove(profile_id);
        if let Err(error) = crate::cloud_ai::save_ai_profiles(&self.cloud_ai.profiles) {
            self.cloud_ai.status = Some(crate::localization::format(
                "settings.ai.message.remove_profile_save_failed",
                &[("error", &(error).to_string())],
            ));
        } else {
            self.cloud_ai.status = Some(crate::localization::format(
                "settings.ai.message.removed",
                &[("name", &(profile.name).to_string())],
            ));
        }
        self.ai_playground.update(cx, |playground, cx| {
            playground.refresh_cloud_profiles(&self.cloud_ai.profiles, cx);
        });
        cx.notify();
    }

    pub(super) fn set_app_ai_profile(
        &mut self,
        profile_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.cloud_ai.profiles.active_profile_id = profile_id.clone();
        if let Err(error) = crate::cloud_ai::save_ai_profiles(&self.cloud_ai.profiles) {
            self.cloud_ai.status = Some(crate::localization::format(
                "settings.ai.message.save_app_ai_backend_failed",
                &[("error", &(error).to_string())],
            ));
        } else {
            self.cloud_ai.status = Some(match profile_id {
                Some(id) => {
                    let name = self
                        .cloud_ai
                        .profiles
                        .profiles
                        .iter()
                        .find(|profile| profile.id == id)
                        .map(|profile| profile.name.as_str())
                        .unwrap_or("profile");
                    crate::localization::format(
                        "settings.ai.message.app_ai_backend_after_restart",
                        &[("name", &(name).to_string())],
                    )
                }
                None => crate::localization::text("settings.ai.option.app_ai_stays_local_runtime"),
            });
        }
        cx.notify();
    }

    // ============================================================
    // AI > Playground
    // ============================================================

    pub(super) fn render_ai_playground(&self) -> AnyElement {
        div().child(self.ai_playground.clone()).into_any_element()
    }

    // The `noop` / `noop_checkbox` / `noop_click` handlers used to live here.
    // They backed 39 controls across SQL Formatting, AI > On device, FIM, and
    // the former Playground placeholder: every one rendered as a working control and did nothing.
    // Their absence is load-bearing — if a control has no implementation, the
    // pane now says so (`render_coming_soon`) rather than miming one.

    // ---- Cloud AI pane actions ----

    pub(super) fn handle_save_cloud_key(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self
            .cloud_ai
            .key_input
            .get(profile_id)
            .cloned()
            .unwrap_or_default();
        if key.trim().is_empty() {
            self.cloud_ai.status = Some(
                crate::localization::text("settings.ai.static.enter_an_api_key_first_2")
                    .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(profile) = self
            .cloud_ai
            .profiles
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            return;
        };
        match crate::cloud_ai::store_api_key_ref(&profile.api_key_ref, &key) {
            Ok(()) => {
                self.cloud_ai.key_input.remove(profile_id);
                self.set_input_value("ai_profile_key", "", window, cx);
                self.cloud_ai.key_saved.insert(profile_id.to_string(), true);
                self.cloud_ai.status = Some(crate::localization::format(
                    "settings.ai.message.key_saved_keychain",
                    &[("name", &(profile.name).to_string())],
                ));
            }
            Err(e) => {
                self.cloud_ai
                    .key_saved
                    .insert(profile_id.to_string(), false);
                self.cloud_ai.status = Some(crate::localization::format(
                    "settings.ai.message.save_profile_key_failed",
                    &[
                        ("name", &(profile.name).to_string()),
                        ("e", &(e).to_string()),
                    ],
                ));
            }
        }
        cx.notify();
    }

    pub(super) fn handle_remove_cloud_key(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self
            .cloud_ai
            .profiles
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            return;
        };
        match crate::cloud_ai::delete_api_key_ref(&profile.api_key_ref) {
            Ok(()) => {
                self.cloud_ai
                    .key_saved
                    .insert(profile_id.to_string(), false);
                self.cloud_ai.key_input.remove(profile_id);
                self.set_input_value("ai_profile_key", "", window, cx);
                self.cloud_ai.status = Some(crate::localization::format(
                    "settings.ai.message.key_removed_from_keychain",
                    &[("name", &(profile.name).to_string())],
                ));
            }
            Err(e) => {
                self.cloud_ai.status = Some(crate::localization::format(
                    "settings.ai.message.could_not_remove_key_e_2",
                    &[
                        ("name", &(profile.name).to_string()),
                        ("e", &(e).to_string()),
                    ],
                ));
            }
        }
        cx.notify();
    }

    pub(super) fn handle_fetch_cloud_models(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        if self.cloud_ai.fetching {
            return;
        }
        let Some(profile) = self
            .cloud_ai
            .profiles
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            return;
        };

        let typed = self
            .cloud_ai
            .key_input
            .get(profile_id)
            .cloned()
            .unwrap_or_default();
        let api_key = if !typed.trim().is_empty() {
            typed
        } else {
            match crate::cloud_ai::load_api_key_ref(&profile.api_key_ref) {
                Ok(Some(key)) => key,
                Ok(None) if profile.kind == crate::cloud_ai::ProviderKind::OpenAiCompatible => {
                    String::new()
                }
                Ok(None) => {
                    self.cloud_ai.status = Some(
                        crate::localization::text("settings.ai.static.fetch_models_needs_key")
                            .to_string(),
                    );
                    cx.notify();
                    return;
                }
                Err(e) => {
                    self.cloud_ai.status = Some(crate::localization::format(
                        "settings.ai.message.could_not_read_key_e_2",
                        &[("e", &(e).to_string())],
                    ));
                    cx.notify();
                    return;
                }
            }
        };

        self.cloud_ai.fetching = true;
        self.cloud_ai.fetch_generation += 1;
        let generation = self.cloud_ai.fetch_generation;
        self.cloud_ai.status = Some(crate::localization::format(
            "settings.ai.message.fetching_models_from",
            &[("name", &(profile.name).to_string())],
        ));
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<String>, String>>();
        let profile_for_task = profile.clone();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result =
                        crate::cloud_ai::fetch_models_for(&profile_for_task, &api_key).await;
                    let mapped = result.map_err(|e| e.to_string());
                    let _ = tx.send(mapped);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        let profile_id = profile_id.to_string();
        let provider_name = profile.name.clone();
        cx.spawn(async move |_this, _cx| {
            if let Ok(result) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        if this.cloud_ai.fetch_generation != generation {
                            return;
                        }
                        this.cloud_ai.fetching = false;
                        match result {
                            Ok(models) => {
                                if let Some(profile) = this
                                    .cloud_ai
                                    .profiles
                                    .profiles
                                    .iter_mut()
                                    .find(|profile| profile.id == profile_id)
                                {
                                    profile.available_model_ids = models.clone();
                                    if profile.model.trim().is_empty() {
                                        profile.model = models.first().cloned().unwrap_or_default();
                                    }
                                }
                                this.ai_playground.update(cx, |playground, cx| {
                                    playground.set_cloud_models(&profile_id, models.clone(), cx)
                                });
                                this.cloud_ai.status = Some(crate::localization::format(
                                    "settings.ai.message.models_found",
                                    &[
                                        ("value", &(models.len()).to_string()),
                                        ("provider_name", &(provider_name).to_string()),
                                    ],
                                ));
                                let _ = crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                            }
                            Err(e) => {
                                this.cloud_ai.status = Some(e);
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    pub(super) fn handle_test_cloud_latency(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        if self.cloud_ai.testing {
            return;
        }
        let Some(profile) = self
            .cloud_ai
            .profiles
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            return;
        };

        let typed = self
            .cloud_ai
            .key_input
            .get(profile_id)
            .cloned()
            .unwrap_or_default();
        let api_key = if !typed.trim().is_empty() {
            typed
        } else {
            match crate::cloud_ai::load_api_key_ref(&profile.api_key_ref) {
                Ok(Some(key)) => key,
                Ok(None) if profile.kind == crate::cloud_ai::ProviderKind::OpenAiCompatible => {
                    String::new()
                }
                Ok(None) => {
                    self.cloud_ai.status = Some(
                        crate::localization::text("settings.ai.static.latency_test_needs_key")
                            .to_string(),
                    );
                    cx.notify();
                    return;
                }
                Err(e) => {
                    self.cloud_ai.status = Some(crate::localization::format(
                        "settings.ai.message.could_not_read_key_e_3",
                        &[("e", &(e).to_string())],
                    ));
                    cx.notify();
                    return;
                }
            }
        };

        self.cloud_ai.testing = true;
        self.cloud_ai.test_generation += 1;
        let generation = self.cloud_ai.test_generation;
        self.cloud_ai.status = Some(crate::localization::format(
            "settings.ai.message.testing_latency",
            &[("name", &(profile.name).to_string())],
        ));
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<std::time::Duration, String>>();
        let profile_for_task = profile.clone();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = crate::cloud_ai::test_latency(&profile_for_task, &api_key).await;
                    let mapped = result.map_err(|e| e.to_string());
                    let _ = tx.send(mapped);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        let profile_id = profile_id.to_string();
        cx.spawn(async move |_this, _cx| {
            if let Ok(result) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        if this.cloud_ai.test_generation != generation {
                            return;
                        }
                        this.cloud_ai.testing = false;
                        match result {
                            Ok(duration) => {
                                this.cloud_ai.latency.insert(profile_id.clone(), duration);
                                this.cloud_ai.status = None;
                            }
                            Err(e) => {
                                this.cloud_ai.status = Some(e);
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    // ============================================================
    // AI > Coding agent CLI
    // ============================================================

    pub(super) fn render_ai_coding_agent_cli(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use sqlly_ai::catalog::CliAgentKind;

        if !crate::distribution::can_launch_user_programs() {
            let _ = (window, cx);
            return div()
                .flex()
                .flex_col()
                .gap(px(Theme::scaled(10.0)))
                .child(self.heading(&crate::localization::text(
                    "settings.ai.heading.coding_agent_cli",
                )))
                .child(crate::app_store_edition_dialog::edition_notice(
                    &crate::localization::text(
                        "settings.ai.coding_agent_cli.app_store_unavailable",
                    ),
                ))
                .into_any_element();
        }

        let config = &self.coding_agent_cli.config;
        let kind = config.kind.unwrap_or(CliAgentKind::ClaudeCode);
        let checking = self.coding_agent_cli.checking;

        let mut children = vec![
            self.heading(&crate::localization::text(
                "settings.ai.heading.coding_agent_cli_2",
            )),
            self.intro(&crate::localization::text(
                "settings.ai.intro.coding_agent_cli",
            )),
            self.checkbox(
                "cli_agent_enabled",
                &crate::localization::text("settings.ai.checkbox.use_local_agent_cli"),
                config.enabled,
                cx,
                |this, cx| {
                    this.coding_agent_cli.config.enabled = !this.coding_agent_cli.config.enabled;
                    cx.notify();
                },
            ),
            self.enum_select_in(
                "cli_agent_kind",
                kind,
                vec![CliAgentKind::ClaudeCode, CliAgentKind::Codex],
                |value| value.display_name().to_string(),
                |this, value, _window, cx| {
                    this.coding_agent_cli.config.kind = Some(value);
                    cx.notify();
                },
                window,
                cx,
            ),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.executable"),
                div()
                    .min_w(px(Theme::scaled(280.)))
                    .child(self.prefs_input("cli_agent_exe"))
                    .into_any_element(),
            ),
            div()
                .flex()
                .flex_row()
                .gap(px(Theme::scaled(8.)))
                .child(self.quiet_button(
                    "cli_agent_detect",
                    &crate::localization::text("settings.ai.quiet_button.detect"),
                    cx,
                    {
                        move |this, window, cx| {
                            this.handle_detect_cli_agent(window, cx);
                        }
                    },
                ))
                .into_any_element(),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.extra_environment"),
                div()
                    .min_w(px(Theme::scaled(280.)))
                    .child(self.prefs_input("cli_agent_env"))
                    .into_any_element(),
            ),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.extra_arguments"),
                div()
                    .min_w(px(Theme::scaled(280.)))
                    .child(self.prefs_input("cli_agent_extra_args"))
                    .into_any_element(),
            ),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.max_turns"),
                div()
                    .min_w(px(Theme::scaled(120.)))
                    .child(self.prefs_input("cli_agent_max_turns"))
                    .into_any_element(),
            ),
            self.labeled_row(
                &crate::localization::text("settings.ai.labeled_row.timeout_minutes"),
                div()
                    .min_w(px(Theme::scaled(120.)))
                    .child(self.prefs_input("cli_agent_timeout"))
                    .into_any_element(),
            ),
            div()
                .flex()
                .flex_row()
                .child(self.quiet_button(
                    "cli_agent_check",
                    &if checking {
                        crate::localization::text("settings.ai.static.checking_2")
                    } else {
                        crate::localization::text("settings.ai.computed.check_2")
                    },
                    cx,
                    |this, _window, cx| {
                        this.handle_check_cli_agent(cx);
                    },
                ))
                .into_any_element(),
        ];

        if let Some(detail) = &self.coding_agent_cli.health_detail {
            let color = if self.coding_agent_cli.health_ok.unwrap_or(false) {
                Theme::success()
            } else {
                Theme::error()
            };
            children.push(
                div()
                    .text_size(px(Theme::font_size_ratio(0.67)))
                    .text_color(color)
                    .child(detail.clone())
                    .into_any_element(),
            );
        }
        if let Some(status) = &self.coding_agent_cli.status {
            children.push(self.hint(status));
        }
        children.push(self.quiet_button(
            "cli_agent_save",
            &crate::localization::text("settings.ai.quiet_button.save"),
            cx,
            |this, _window, cx| {
                this.handle_save_cli_agent(cx);
            },
        ));

        self.pane_container(children)
    }

    fn handle_detect_cli_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use sqlly_ai::catalog::CliAgentKind;
        let kind = self
            .coding_agent_cli
            .config
            .kind
            .unwrap_or(CliAgentKind::ClaudeCode);
        #[cfg(not(target_arch = "wasm32"))]
        {
            use gpui_component::WindowExt as _;
            if let Some(path) = crate::local_cli_agent::detect_executable(kind) {
                let path_str = path.to_string_lossy().into_owned();
                self.coding_agent_cli.config.exe_path = path_str.clone();
                self.set_input_value("cli_agent_exe", &path_str, window, cx);
                self.coding_agent_cli.status = Some(
                    crate::localization::text("settings.ai.static.executable_path_updated")
                        .to_string(),
                );
            } else {
                window.push_notification(
                    gpui_component::notification::Notification::warning(crate::localization::text(
                        "settings.ai.coding_agent_cli.executable_not_found",
                    )),
                    cx,
                );
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            self.coding_agent_cli.status = Some(crate::browser_preview::unavailable(
                &crate::localization::text(
                    "settings.ai.coding_agent_cli.browser_unavailable_status",
                ),
            ));
        }
        cx.notify();
    }

    fn handle_check_cli_agent(&mut self, cx: &mut Context<Self>) {
        use sqlly_ai::catalog::CliAgentKind;
        if self.coding_agent_cli.checking {
            return;
        }
        let kind = self
            .coding_agent_cli
            .config
            .kind
            .unwrap_or(CliAgentKind::ClaudeCode);
        let exe = self.coding_agent_cli.config.exe_path.clone();
        self.coding_agent_cli.checking = true;
        self.coding_agent_cli.check_generation += 1;
        let generation = self.coding_agent_cli.check_generation;
        self.coding_agent_cli.health_detail =
            Some(crate::localization::text("settings.ai.static.checking_3").to_string());
        cx.notify();

        #[cfg(not(target_arch = "wasm32"))]
        {
            use sqlly_ai::catalog::McpServerSpec;
            let mcp = McpServerSpec {
                name: "sqlly".to_string(),
                command: crate::mcp_settings::find_cli_path(),
                args: vec!["mcp".to_string()],
            };
            let (tx, rx) = tokio::sync::oneshot::channel::<crate::local_cli_agent::HealthReport>();
            let exe_for_task = exe.clone();
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        let report = crate::local_cli_agent::health_check(
                            kind,
                            std::path::Path::new(&exe_for_task),
                            &mcp,
                        )
                        .await;
                        let _ = tx.send(report);
                    })
                    .await
                    .ok();
            })
            .detach();

            let async_cx = cx.to_async();
            let weak = cx.weak_entity();
            cx.spawn(async move |_this, _cx| {
                if let Ok(report) = rx.await {
                    let mut cx = async_cx.clone();
                    weak.update(&mut cx, |this, cx| {
                        if this.coding_agent_cli.check_generation != generation {
                            return;
                        }
                        this.coding_agent_cli.checking = false;
                        this.coding_agent_cli.health_ok = Some(
                            report.found && report.mcp_ok.unwrap_or(kind == CliAgentKind::Codex),
                        );
                        this.coding_agent_cli.health_detail =
                            Some(crate::localization::message(report.detail));
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.coding_agent_cli.checking = false;
            self.coding_agent_cli.health_ok = Some(false);
            self.coding_agent_cli.health_detail = Some(crate::browser_preview::unavailable(
                &crate::localization::text(
                    "settings.ai.coding_agent_cli.browser_unavailable_health",
                ),
            ));
        }
    }

    fn handle_save_cli_agent(&mut self, cx: &mut Context<Self>) {
        let config = self.coding_agent_cli.config.clone();
        match crate::local_cli_agent_config::save_local_cli_agent_config(&config) {
            Ok(()) => {
                self.coding_agent_cli.status =
                    Some(crate::localization::text("settings.ai.static.saved").to_string());
            }
            Err(error) => {
                self.coding_agent_cli.status = Some(error);
            }
        }
        cx.notify();
    }
}

/// One `Noul` question over the state `"ping"`, timed. Free function so the
/// tokio task it runs in owns no UI state.
#[cfg(not(target_arch = "wasm32"))]
async fn test_decision_model(
    config: &crate::decision_ai::DecisionAiConfig,
    api_key: String,
) -> Result<std::time::Duration, String> {
    use sqlly_ai::catalog::{DecisionProvider, DecisionRequest, Question};

    let jev_config = sqlly_ai::jev::JevConfig {
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        ..sqlly_ai::jev::JevConfig::default()
    };
    let client = sqlly_ai::jev::JevClient::new(&jev_config, api_key).map_err(|e| e.to_string())?;
    let mut questions = std::collections::BTreeMap::new();
    questions.insert(
        "ready".to_string(),
        Question::Noul {
            instructions: "Is the state the word ping?".to_string(),
        },
    );
    let request = DecisionRequest {
        state: serde_json::Value::String("ping".to_string()),
        questions,
    };
    let started = std::time::Instant::now();
    client.decide(request).await.map_err(|e| e.to_string())?;
    Ok(started.elapsed())
}

#[cfg(test)]
mod decision_pane_tests {
    use crate::preferences::{PreferencesPane, build_preferences_tree};

    #[test]
    fn decision_model_pane_is_in_ai_folder() {
        let tree = build_preferences_tree();
        let ai = tree
            .iter()
            .find(|node| node.title == "AI")
            .expect("AI folder");
        assert!(
            ai.children
                .iter()
                .any(|child| child.pane == Some(PreferencesPane::AiDecisionModel)),
            "Decision model pane missing from AI folder"
        );
    }
}

#[cfg(test)]
mod coding_agent_tests {
    use crate::preferences::{PreferencesPane, build_preferences_tree};
    use sqlly_ai::catalog::CliAgentKind;

    #[test]
    fn coding_agent_pane_is_in_ai_folder() {
        let tree = build_preferences_tree();
        let ai = tree
            .iter()
            .find(|node| node.title == "AI")
            .expect("AI folder");
        assert!(
            ai.children
                .iter()
                .any(|child| child.pane == Some(PreferencesPane::AiCodingAgentCli)),
            "Coding agent CLI pane missing from AI folder"
        );
    }

    #[test]
    fn egress_label_names_vendor_for_cli_ids() {
        let _guard = crate::localization::test_language_with(
            crate::localization::Language::English,
            Some(include_str!(
                "../../../../crates/sqlly-i18n/translations/en.json"
            )),
        );
        let label = crate::ai_panel::agent_egress_label("cli:claude-code");
        assert!(label.contains("local Claude Code CLI"));
        assert!(label.contains("Anthropic's cloud"));
        let label = crate::ai_panel::agent_egress_label("cli:codex");
        assert!(label.contains("local Codex CLI"));
        assert!(label.contains("OpenAI's cloud"));
        let _ = CliAgentKind::ClaudeCode.provider_id();
    }
}
