//! Ephemeral single-request AI Playground used by Settings ▸ AI ▸ Playground.
//!
//! The view deliberately owns every prompt, response, metric, and task handle
//! in memory. Closing Settings drops the entity and cancels any request; no
//! prompt or response is written to preferences, history, logs, or catalogs.

use std::collections::BTreeMap;
use std::time::Duration;

use gpui::{
    ClipboardItem, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_ai::catalog::{
    ChatMessage, ChatRole, CompletionRequest, CompletionResponse, DynLlmProvider, ModelCapability,
};

use crate::cloud_ai::{AiProfilesConfig, ProviderProfile, egress_short, load_ai_profiles};
use crate::theme::Theme;

pub const MAX_SYSTEM_CHARS: usize = 8_000;
pub const MAX_PROMPT_CHARS: usize = 16_000;
pub const MAX_RESPONSE_CHARS: usize = 32_000;
pub const DEFAULT_MAX_TOKENS: usize = 512;
const STREAM_CHUNK_CHARS: usize = 192;
const STREAM_CHUNK_DELAY: Duration = Duration::from_millis(6);
const TOKEN_LIMITS: [usize; 5] = [128, 256, 512, 1024, 2048];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaygroundProvider {
    Ollama,
    Mlx,
    Profile(String),
}

impl PlaygroundProvider {
    fn label(&self, profiles: &AiProfilesConfig) -> String {
        match self {
            Self::Ollama => crate::localization::text("ai.playground.provider_ollama"),
            Self::Mlx => crate::localization::text("ai.playground.provider_mlx"),
            Self::Profile(id) => profiles
                .profiles
                .iter()
                .find(|profile| profile.id == *id)
                .map(|profile| {
                    crate::localization::format(
                        "ai.playground.provider_profile",
                        &[("name", &profile.name), ("egress", &egress_short(profile))],
                    )
                })
                .unwrap_or_else(|| {
                    crate::localization::format(
                        "ai.playground.provider_missing_profile",
                        &[("id", id)],
                    )
                }),
        }
    }

    fn local_id(&self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("ollama"),
            Self::Mlx => Some("mlx"),
            Self::Profile(_) => None,
        }
    }

    fn profile_id(&self) -> Option<String> {
        match self {
            Self::Profile(id) => Some(id.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaygroundPhase {
    Idle,
    Running,
    Complete,
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PlaygroundMetrics {
    prompt_tokens: usize,
    response_tokens: usize,
    latency_ms: u128,
}

enum PlaygroundEvent {
    Metrics(PlaygroundMetrics),
    Chunk(String),
    Finished,
    Failed(String),
}

#[derive(Clone)]
struct PlaygroundRun {
    provider: PlaygroundProvider,
    profile: Option<ProviderProfile>,
    model: String,
    system: String,
    prompt: String,
    max_tokens: usize,
    api_key: Option<String>,
}

pub struct AiPlayground {
    provider: PlaygroundProvider,
    models: BTreeMap<PlaygroundProvider, Vec<String>>,
    selected_model: Option<String>,
    max_tokens: usize,
    /// Provider choices in Select order; labels are derived per language.
    provider_values: Vec<PlaygroundProvider>,
    /// Profiles the provider labels were built from.
    provider_profiles: AiProfilesConfig,
    provider_select: Entity<SelectState<Vec<String>>>,
    model_select: Entity<SelectState<Vec<String>>>,
    token_select: Entity<SelectState<Vec<String>>>,
    system_input: Entity<TextareaState>,
    prompt_input: Entity<TextareaState>,
    system_prompt: String,
    user_prompt: String,
    output: String,
    phase: PlaygroundPhase,
    metrics: PlaygroundMetrics,
    generation: u64,
    models_dirty: bool,
    #[cfg(not(target_arch = "wasm32"))]
    abort_handle: Option<tokio::task::AbortHandle>,
    _subscriptions: Vec<Subscription>,
}

impl AiPlayground {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let profiles = load_ai_profiles();
        let models = models_from_preferences(&profiles);
        let provider = first_provider_with_models(&models).unwrap_or(PlaygroundProvider::Ollama);
        let selected_model = models
            .get(&provider)
            .and_then(|models| models.first())
            .cloned();
        // The choice list is held as provider values in a fixed order; the
        // Select shows their labels, rebound on a language switch, and a
        // confirmed choice maps back to its value by index.
        let provider_values = provider_values(&profiles);
        let label_profiles = profiles.clone();
        let label_values = provider_values.clone();
        let provider_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, move || {
                    provider_labels(&label_values, &label_profiles)
                }),
                None,
                window,
                cx,
            )
        });
        if let Some(index) = provider_values.iter().position(|value| *value == provider) {
            provider_select.update(cx, |state, cx| {
                state.set_selected_index(Some(gpui_component::IndexPath::new(index)), window, cx)
            });
        }

        let model_items = models.get(&provider).cloned().unwrap_or_default();
        let model_select = cx.new(|cx| SelectState::new(model_items, None, window, cx));
        if let Some(model) = selected_model.as_ref() {
            model_select.update(cx, |state, cx| state.set_selected_value(model, window, cx));
        }

        let token_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, token_limit_labels),
                None,
                window,
                cx,
            )
        });
        if let Some(index) = TOKEN_LIMITS
            .iter()
            .position(|tokens| *tokens == DEFAULT_MAX_TOKENS)
        {
            token_select.update(cx, |state, cx| {
                state.set_selected_index(Some(gpui_component::IndexPath::new(index)), window, cx)
            });
        }

        let system_input = cx.new(|cx| {
            TextareaState::new(window, cx).soft_wrap(true).placeholder(
                crate::localization::input_placeholder(
                    window,
                    cx,
                    "ai.playground.system_prompt_placeholder",
                ),
            )
        });
        let prompt_input = cx.new(|cx| {
            TextareaState::new(window, cx).soft_wrap(true).placeholder(
                crate::localization::input_placeholder(
                    window,
                    cx,
                    "ai.playground.prompt_placeholder",
                ),
            )
        });

        let subscriptions = vec![
            cx.subscribe_in(
                &provider_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    let labels = provider_labels(&this.provider_values, &this.provider_profiles);
                    let index = selected_choice_index(
                        select.read(cx).selected_index(cx).map(|index| index.row),
                        value.as_deref(),
                        &labels,
                    );
                    let Some(provider) = index.and_then(|index| this.provider_values.get(index))
                    else {
                        return;
                    };
                    let provider = provider.clone();
                    this.provider = provider;
                    this.sync_model_select(window, cx);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &model_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.selected_model = value.as_ref().map(ToString::to_string);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &token_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if let Some(tokens) = selected_choice_index(
                        select.read(cx).selected_index(cx).map(|index| index.row),
                        value.as_deref(),
                        &token_limit_labels(),
                    )
                    .and_then(|index| TOKEN_LIMITS.get(index))
                    {
                        this.max_tokens = *tokens;
                    }
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &system_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.system_prompt = input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &prompt_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.user_prompt = input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
        ];

        Self {
            provider,
            models,
            selected_model,
            max_tokens: DEFAULT_MAX_TOKENS,
            provider_values,
            provider_profiles: profiles,
            provider_select,
            model_select,
            token_select,
            system_input,
            prompt_input,
            system_prompt: String::new(),
            user_prompt: String::new(),
            output: String::new(),
            phase: PlaygroundPhase::Idle,
            metrics: PlaygroundMetrics::default(),
            generation: 0,
            models_dirty: false,
            #[cfg(not(target_arch = "wasm32"))]
            abort_handle: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_models(
        &mut self,
        provider: PlaygroundProvider,
        mut models: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        models.sort();
        models.dedup();
        self.models.insert(provider, models);
        self.models_dirty = true;
        cx.notify();
    }

    pub fn set_cloud_models(
        &mut self,
        profile_id: &str,
        models: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.set_models(
            PlaygroundProvider::Profile(profile_id.to_string()),
            models,
            cx,
        );
    }

    pub fn refresh_cloud_profiles(&mut self, profiles: &AiProfilesConfig, cx: &mut Context<Self>) {
        for profile in &profiles.profiles {
            self.models.insert(
                PlaygroundProvider::Profile(profile.id.clone()),
                profile.available_model_ids.clone(),
            );
        }
        self.models_dirty = true;
        cx.notify();
    }

    pub fn set_ollama_models(&mut self, models: Vec<String>, cx: &mut Context<Self>) {
        self.set_models(PlaygroundProvider::Ollama, models, cx);
    }

    fn sync_model_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let models = self.models.get(&self.provider).cloned().unwrap_or_default();
        let selected = self
            .selected_model
            .clone()
            .filter(|selected| models.contains(selected))
            .or_else(|| models.first().cloned());
        self.selected_model = selected.clone();
        self.model_select.update(cx, |state, cx| {
            state.set_items(models, window, cx);
            state.set_selected_index(None, window, cx);
            if let Some(selected) = selected.as_ref() {
                state.set_selected_value(selected, window, cx);
            }
        });
        self.models_dirty = false;
    }

    fn validate_run(&self) -> Result<PlaygroundRun, String> {
        let prompt = self.user_prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(crate::localization::text("ai.playground.prompt_required"));
        }
        if self.system_prompt.chars().count() > MAX_SYSTEM_CHARS {
            return Err(crate::localization::format(
                "ai.playground.system_prompt_too_long",
                &[("limit", &MAX_SYSTEM_CHARS.to_string())],
            ));
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(crate::localization::format(
                "ai.playground.prompt_too_long",
                &[("limit", &MAX_PROMPT_CHARS.to_string())],
            ));
        }
        let model = self
            .selected_model
            .clone()
            .ok_or_else(|| crate::localization::text("ai.playground.model_required"))?;
        let api_key = match self.provider.profile_id() {
            Some(profile_id) => {
                let profiles = load_ai_profiles();
                let Some(profile) = profiles.profiles.iter().find(|p| p.id == profile_id) else {
                    return Err(crate::localization::text("ai.playground.profile_missing"));
                };
                match crate::cloud_ai::load_api_key_ref(&profile.api_key_ref) {
                    Ok(Some(key)) if !key.trim().is_empty() => Some(key),
                    Ok(None) if profile.kind == crate::cloud_ai::ProviderKind::OpenAiCompatible => {
                        Some(String::new())
                    }
                    Ok(_) => {
                        return Err(crate::localization::format(
                            "ai.playground.api_key_required",
                            &[("provider", &profile.name)],
                        ));
                    }
                    Err(error) => {
                        return Err(crate::localization::format(
                            "ai.playground.api_key_read_failed",
                            &[("error", &error.to_string())],
                        ));
                    }
                }
            }
            None => None,
        };
        let profile = self.provider.profile_id().and_then(|profile_id| {
            load_ai_profiles()
                .profiles
                .into_iter()
                .find(|profile| profile.id == profile_id)
        });
        Ok(PlaygroundRun {
            provider: self.provider.clone(),
            profile,
            model,
            system: self.system_prompt.trim().to_string(),
            prompt,
            max_tokens: self.max_tokens.min(2048),
            api_key,
        })
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        let run = match self.validate_run() {
            Ok(run) => run,
            Err(error) => {
                self.phase = PlaygroundPhase::Failed(error);
                cx.notify();
                return;
            }
        };

        #[cfg(target_arch = "wasm32")]
        {
            let _ = run;
            self.phase = PlaygroundPhase::Failed(crate::browser_preview::unavailable(
                &crate::localization::text("ai.playground.browser_unavailable"),
            ));
            cx.notify();
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            self.cancel_running();
            self.generation += 1;
            let generation = self.generation;
            self.output.clear();
            self.metrics = PlaygroundMetrics {
                prompt_tokens: estimate_tokens(&format!("{}\n{}", run.system, run.prompt)),
                response_tokens: 0,
                latency_ms: 0,
            };
            self.phase = PlaygroundPhase::Running;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<PlaygroundEvent>(16);
            let task = crate::tokio_handle().spawn(run_request(run, tx));
            self.abort_handle = Some(task.abort_handle());

            let async_cx = cx.to_async();
            let weak_self = cx.weak_entity();
            cx.spawn(async move |_this, _cx| {
                while let Some(event) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let should_continue = weak_self
                        .update(&mut cx, |this, cx| {
                            if this.generation != generation {
                                return false;
                            }
                            match event {
                                PlaygroundEvent::Metrics(metrics) => this.metrics = metrics,
                                PlaygroundEvent::Chunk(chunk) => {
                                    this.output.push_str(&chunk);
                                    this.metrics.response_tokens = estimate_tokens(&this.output);
                                }
                                PlaygroundEvent::Finished => {
                                    this.phase = PlaygroundPhase::Complete;
                                    this.abort_handle = None;
                                }
                                PlaygroundEvent::Failed(error) => {
                                    this.phase = PlaygroundPhase::Failed(error);
                                    this.abort_handle = None;
                                }
                            }
                            cx.notify();
                            matches!(this.phase, PlaygroundPhase::Running)
                        })
                        .unwrap_or(false);
                    if !should_continue {
                        break;
                    }
                }
            })
            .detach();
            cx.notify();
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.phase, PlaygroundPhase::Running) {
            return;
        }
        self.cancel_running();
        self.generation += 1;
        self.phase = PlaygroundPhase::Cancelled;
        cx.notify();
    }

    fn cancel_running(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(handle) = self.abort_handle.take() {
            handle.abort();
        }
    }

    fn status_text(&self) -> String {
        let key = match &self.phase {
            PlaygroundPhase::Idle => "ai.playground.status_idle",
            PlaygroundPhase::Running => "ai.playground.status_running",
            PlaygroundPhase::Complete => "ai.playground.status_complete",
            PlaygroundPhase::Cancelled => "ai.playground.status_cancelled",
            PlaygroundPhase::Failed(error) => return error.clone(),
        };
        crate::localization::text(key)
    }
}

impl Drop for AiPlayground {
    fn drop(&mut self) {
        self.cancel_running();
    }
}

impl Render for AiPlayground {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.models_dirty {
            self.sync_model_select(window, cx);
        }
        let running = matches!(self.phase, PlaygroundPhase::Running);
        let can_run = !running
            && !self.user_prompt.trim().is_empty()
            && self.selected_model.is_some()
            && self.user_prompt.chars().count() <= MAX_PROMPT_CHARS
            && self.system_prompt.chars().count() <= MAX_SYSTEM_CHARS;
        let status_is_error = matches!(self.phase, PlaygroundPhase::Failed(_));
        let output = if self.output.is_empty() {
            crate::localization::text("ai.playground.response_placeholder")
        } else {
            self.output.clone()
        };

        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(8.0)))
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text())
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("ai.playground.introduction")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.0)))
                    .child(
                        div()
                            .w(px(Theme::scaled(200.0)))
                            .child(Select::new(&self.provider_select).w_full()),
                    )
                    .child(
                        div().w(px(Theme::scaled(260.0))).child(
                            Select::new(&self.model_select)
                                .placeholder(crate::localization::text(
                                    "ai.playground.no_models_available",
                                ))
                                .w_full(),
                        ),
                    )
                    .child(
                        div()
                            .w(px(Theme::scaled(130.0)))
                            .child(Select::new(&self.token_select).w_full()),
                    ),
            )
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .child(crate::localization::text("ai.playground.cloud_disclosure")),
            )
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(crate::localization::text(
                        "ai.playground.system_prompt_label",
                    )),
            )
            .child(
                div()
                    .h(px(Theme::scaled(78.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(Theme::scaled(4.0)))
                    .text_color(Theme::text())
                    .child(Textarea::new(&self.system_input)),
            )
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .child(crate::localization::format(
                        "ai.playground.system_character_count",
                        &[
                            ("count", &self.system_prompt.chars().count().to_string()),
                            ("limit", &MAX_SYSTEM_CHARS.to_string()),
                        ],
                    )),
            )
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(crate::localization::text("ai.playground.user_prompt_label")),
            )
            .child(
                div()
                    .h(px(Theme::scaled(108.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(Theme::scaled(4.0)))
                    .text_color(Theme::text())
                    .child(Textarea::new(&self.prompt_input)),
            )
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .child(crate::localization::format(
                        "ai.playground.prompt_character_count",
                        &[
                            ("count", &self.user_prompt.chars().count().to_string()),
                            ("limit", &MAX_PROMPT_CHARS.to_string()),
                        ],
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.0)))
                    .child(
                        Button::new("playground-run")
                            .small()
                            .label(crate::localization::text("ai.playground.run_button"))
                            .disabled(!can_run)
                            .on_click(cx.listener(|this, _, _window, cx| this.run(cx))),
                    )
                    .when(running, |row| {
                        row.child(
                            Button::new("playground-cancel")
                                .small()
                                .label(crate::localization::text("ai.playground.cancel_button"))
                                .on_click(cx.listener(|this, _, _window, cx| this.cancel(cx))),
                        )
                    })
                    .when(!self.output.is_empty(), |row| {
                        let output = self.output.clone();
                        row.child(
                            Button::new("playground-copy")
                                .small()
                                .label(crate::localization::text(
                                    "ai.playground.copy_response_button",
                                ))
                                .on_click(move |_, _window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        output.clone(),
                                    ));
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .text_color(if status_is_error {
                        Theme::warning()
                    } else {
                        Theme::text_dim()
                    })
                    .child(self.status_text()),
            )
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(crate::localization::text("ai.playground.response_label")),
            )
            .child(
                div()
                    .min_h(px(Theme::scaled(120.0)))
                    .max_h(px(Theme::scaled(260.0)))
                    .overflow_y_scrollbar()
                    .p(px(Theme::scaled(10.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(Theme::scaled(4.0)))
                    .bg(Theme::bg())
                    .text_color(if self.output.is_empty() {
                        Theme::text_dim()
                    } else {
                        Theme::text()
                    })
                    .child(output),
            )
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .child(crate::localization::format(
                        "ai.playground.metrics",
                        &[
                            ("prompt_tokens", &self.metrics.prompt_tokens.to_string()),
                            ("response_tokens", &self.metrics.response_tokens.to_string()),
                            ("latency_ms", &self.metrics.latency_ms.to_string()),
                            ("response_cap", &MAX_RESPONSE_CHARS.to_string()),
                        ],
                    )),
            )
    }
}

fn models_from_preferences(
    profiles: &AiProfilesConfig,
) -> BTreeMap<PlaygroundProvider, Vec<String>> {
    let mut models = BTreeMap::new();
    let summary = crate::preferences::AiSummaryPreferences::load();
    let ollama_model = summary
        .model_override()
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            crate::fim_completion::FimAiModel::Qwen25Coder15BBase
                .ollama_model_name()
                .to_string()
        });
    models.insert(PlaygroundProvider::Ollama, vec![ollama_model]);

    let mlx_model = std::env::var("SQLLY_LLM_MODEL")
        .ok()
        .filter(|_| std::env::var("SQLLY_LLM_PROVIDER").ok().as_deref() == Some("mlx"))
        .or_else(|| std::env::var("SQLLY_MLX_CHAT_MODEL").ok())
        .filter(|model| !model.trim().is_empty());
    models.insert(PlaygroundProvider::Mlx, mlx_model.into_iter().collect());

    for profile in &profiles.profiles {
        models.insert(
            PlaygroundProvider::Profile(profile.id.clone()),
            prioritize_model(
                profile.available_model_ids.clone(),
                Some(profile.model.as_str()).filter(|model| !model.is_empty()),
            ),
        );
    }
    models
}

/// Provider choices in the order the Select lists them.
fn provider_values(profiles: &AiProfilesConfig) -> Vec<PlaygroundProvider> {
    let mut values = vec![PlaygroundProvider::Ollama, PlaygroundProvider::Mlx];
    values.extend(
        profiles
            .profiles
            .iter()
            .map(|profile| PlaygroundProvider::Profile(profile.id.clone())),
    );
    values
}

/// Current-language labels for `values`, index for index.
fn provider_labels(values: &[PlaygroundProvider], profiles: &AiProfilesConfig) -> Vec<String> {
    values.iter().map(|value| value.label(profiles)).collect()
}

/// Current-language labels for [`TOKEN_LIMITS`], index for index.
fn token_limit_labels() -> Vec<String> {
    TOKEN_LIMITS
        .iter()
        .map(|tokens| {
            crate::localization::format(
                "ai.playground.token_limit_option",
                &[("count", &tokens.to_string())],
            )
        })
        .collect()
}

/// The business index of a confirmed Select choice: the Select's own index
/// when it has one in range, else the position of the confirmed label in the
/// current-language label list. Never parses the label's text.
fn selected_choice_index(
    selected_row: Option<usize>,
    label: Option<&str>,
    labels: &[String],
) -> Option<usize> {
    selected_row
        .filter(|row| *row < labels.len())
        .or_else(|| label.and_then(|label| labels.iter().position(|candidate| candidate == label)))
}

fn prioritize_model(mut models: Vec<String>, selected: Option<&str>) -> Vec<String> {
    if let Some(selected) = selected
        && let Some(index) = models.iter().position(|model| model == selected)
    {
        let selected = models.remove(index);
        models.insert(0, selected);
    }
    models
}

fn first_provider_with_models(
    models: &BTreeMap<PlaygroundProvider, Vec<String>>,
) -> Option<PlaygroundProvider> {
    models
        .iter()
        .find(|(_, models)| !models.is_empty())
        .map(|(provider, _)| provider.clone())
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn chunks(text: &str, chunk_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        current.push(character);
        if current.chars().count() == chunk_chars {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn completion_request(run: &PlaygroundRun) -> CompletionRequest {
    let mut messages = Vec::with_capacity(2);
    if !run.system.is_empty() {
        messages.push(ChatMessage {
            role: ChatRole::System,
            content: run.system.clone(),
        });
    }
    messages.push(ChatMessage {
        role: ChatRole::User,
        content: run.prompt.clone(),
    });
    CompletionRequest {
        messages,
        model: Some(run.model.clone()),
        capability: Some(ModelCapability::Chat),
        max_tokens: Some(run.max_tokens),
        temperature: None,
        metadata: serde_json::json!({"surface": "playground"}),
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn run_request(run: PlaygroundRun, tx: tokio::sync::mpsc::Sender<PlaygroundEvent>) {
    let started = std::time::Instant::now();
    let request = completion_request(&run);
    let provider = match provider_for_run(&run) {
        Ok(provider) => provider,
        Err(error) => {
            let _ = tx.send(PlaygroundEvent::Failed(error)).await;
            return;
        }
    };
    let response = match provider.complete(request).await {
        Ok(response) => response,
        Err(error) => {
            let _ = tx.send(PlaygroundEvent::Failed(error.to_string())).await;
            return;
        }
    };
    let text = bounded_text(&response.text, MAX_RESPONSE_CHARS);
    let metrics = response_metrics(&run, &response, started.elapsed());
    if tx.send(PlaygroundEvent::Metrics(metrics)).await.is_err() {
        return;
    }
    for chunk in chunks(&text, STREAM_CHUNK_CHARS) {
        if tx.send(PlaygroundEvent::Chunk(chunk)).await.is_err() {
            return;
        }
        tokio::time::sleep(STREAM_CHUNK_DELAY).await;
    }
    let _ = tx.send(PlaygroundEvent::Finished).await;
}

#[cfg(not(target_arch = "wasm32"))]
fn provider_for_run(run: &PlaygroundRun) -> Result<DynLlmProvider, String> {
    if let Some(provider) = run.provider.local_id() {
        return sqlly_services::providers::build_local_llm_provider(provider, &run.model);
    }
    let profile = run
        .profile
        .clone()
        .ok_or_else(|| crate::localization::text("ai.playground.provider_unsupported"))?;
    let api_key = run
        .api_key
        .clone()
        .ok_or_else(|| crate::localization::text("ai.playground.provider_key_missing"))?;
    let spec = crate::cloud_ai::cloud_provider_spec(&profile, api_key)?;
    Ok(std::sync::Arc::new(
        sqlly_services::providers::CloudLlmProvider::new(spec)?,
    ))
}

fn response_metrics(
    run: &PlaygroundRun,
    response: &CompletionResponse,
    latency: Duration,
) -> PlaygroundMetrics {
    let meta_usize = |key: &str| {
        response
            .meta
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize)
    };
    PlaygroundMetrics {
        prompt_tokens: meta_usize("prompt_tokens")
            .or_else(|| meta_usize("prompt_eval_count"))
            .unwrap_or_else(|| estimate_tokens(&format!("{}\n{}", run.system, run.prompt))),
        response_tokens: meta_usize("completion_tokens")
            .or_else(|| meta_usize("eval_count"))
            .unwrap_or_else(|| estimate_tokens(&response.text)),
        latency_ms: latency.as_millis(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_run() -> PlaygroundRun {
        PlaygroundRun {
            provider: PlaygroundProvider::Ollama,
            profile: None,
            model: "gpt-test".to_string(),
            system: "Be concise".to_string(),
            prompt: "Hello".to_string(),
            max_tokens: 256,
            api_key: None,
        }
    }

    #[test]
    fn provider_choice_maps_by_index_in_any_language() {
        let profiles = AiProfilesConfig::default();
        let values = provider_values(&profiles);
        assert_eq!(
            values,
            vec![PlaygroundProvider::Ollama, PlaygroundProvider::Mlx]
        );
        let english = provider_labels(&values, &profiles);
        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let spanish = provider_labels(&values, &profiles);
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
        assert_ne!(english, spanish);
        // A label confirmed in Spanish resolves against the Spanish list to
        // the same provider, and an English label left over from before the
        // switch no longer resolves to anything by text.
        assert_eq!(
            selected_choice_index(None, Some(spanish[1].as_str()), &spanish),
            Some(1)
        );
        assert_eq!(
            selected_choice_index(None, Some(english[1].as_str()), &spanish),
            None
        );
        // The Select's own index wins over text.
        assert_eq!(
            selected_choice_index(Some(0), Some(spanish[1].as_str()), &spanish),
            Some(0)
        );
        assert_eq!(selected_choice_index(Some(9), None, &spanish), None);
    }

    #[test]
    fn token_limits_come_from_the_index_not_the_label_text() {
        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let labels = token_limit_labels();
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
        assert_eq!(labels.len(), TOKEN_LIMITS.len());
        let index = selected_choice_index(None, Some(labels[3].as_str()), &labels).unwrap();
        assert_eq!(TOKEN_LIMITS[index], 1024);
    }

    #[test]
    fn response_bound_is_unicode_safe() {
        assert_eq!(bounded_text("a🦀bc", 2), "a🦀");
        assert_eq!(bounded_text("short", 99), "short");
    }

    #[test]
    fn chunks_reassemble_without_splitting_unicode() {
        let text = "one🦀two三four";
        assert_eq!(chunks(text, 3).concat(), text);
        assert!(
            chunks(text, 3)
                .iter()
                .all(|chunk| chunk.chars().count() <= 3)
        );
    }

    #[test]
    fn completion_request_carries_bounded_token_choice_and_two_roles() {
        let request = completion_request(&sample_run());
        assert_eq!(request.max_tokens, Some(256));
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, ChatRole::System);
        assert_eq!(request.messages[1].role, ChatRole::User);
    }

    #[test]
    fn token_estimate_is_small_bounded_and_nonzero_for_text() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("1234"), 1);
        assert_eq!(estimate_tokens("12345"), 2);
    }

    #[test]
    fn saved_model_selection_is_first_without_duplicating_it() {
        assert_eq!(
            prioritize_model(vec!["a".into(), "b".into(), "c".into()], Some("b")),
            vec!["b", "a", "c"]
        );
        assert_eq!(
            prioritize_model(vec!["a".into(), "b".into()], Some("missing")),
            vec!["a", "b"]
        );
    }
}
