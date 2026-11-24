//! Workbench side of AI cells: the provider and the read-only connection
//! hosts a run gets, the deterministic Review / Explain changes checks, the
//! cloud sharing decision (`notebook_security::authorize`), and pumping the
//! agent's progress back into the notebook. Only slots the notebook's
//! trust authorizes, and that the production read lock leaves open, get a
//! connection host; the agent loop itself refuses everything else again.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::BTreeMap;

use super::*;
#[cfg(not(target_arch = "wasm32"))]
use crate::notebook::view::NotebookView;

/// The model an AI cell runs on, decided when Run is clicked.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum NotebookModel {
    Ready(sqlly_ai::catalog::DynLlmProvider),
    /// Nothing to run on; the message says why and what to set up.
    Unavailable(String),
}

/// The AI profile selected in Settings ▸ AI when there is one, else the
/// app's local runtime. A selected cloud profile that can't be built is
/// reported, never swapped for another model, and the built-in stub (no
/// runtime found) is no model at all.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn notebook_model(
    profiles: &crate::cloud_ai::AiProfilesConfig,
    load_key: impl Fn(&str) -> Result<Option<String>, String>,
    local: sqlly_ai::catalog::DynLlmProvider,
) -> NotebookModel {
    use crate::cloud_ai::ProviderKind;
    use crate::localization::{format, text};
    let active = profiles
        .active_profile_id
        .as_deref()
        .and_then(|id| profiles.profiles.iter().find(|p| p.id == id));
    let Some(profile) = active else {
        if sqlly_services::notebook_agent::is_stub_provider(local.provider_id()) {
            return NotebookModel::Unavailable(text("notebook.ai.host.no_model"));
        }
        return NotebookModel::Ready(local);
    };
    let unavailable = |reason: String| {
        NotebookModel::Unavailable(format(
            "notebook.ai.host.cloud_profile_unavailable",
            &[
                ("profile", profile.name.as_str()),
                ("reason", reason.as_str()),
            ],
        ))
    };
    if profile.model.trim().is_empty() {
        return unavailable(text("notebook.ai.host.reason_no_model"));
    }
    let key = match load_key(&profile.api_key_ref) {
        Ok(Some(key)) => key,
        Ok(None) if profile.kind == ProviderKind::OpenAiCompatible => String::new(),
        Ok(None) => return unavailable(text("notebook.ai.host.reason_no_key")),
        Err(_) => return unavailable(text("notebook.ai.host.reason_key_unreadable")),
    };
    match crate::cloud_ai::cloud_provider_spec(profile, key)
        .and_then(sqlly_services::providers::CloudLlmProvider::new)
    {
        Ok(provider) => NotebookModel::Ready(Arc::new(provider)),
        Err(_) => unavailable(text("notebook.ai.host.reason_invalid")),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
thread_local! {
    /// What tests run AI cells on instead of the real resolver.
    pub(crate) static TEST_NOTEBOOK_MODEL: std::cell::RefCell<Option<sqlly_ai::catalog::DynLlmProvider>> =
        const { std::cell::RefCell::new(None) };
}

/// The model tests put in place of the resolver, if any.
#[cfg(all(test, not(target_arch = "wasm32")))]
fn test_notebook_model() -> Option<sqlly_ai::catalog::DynLlmProvider> {
    TEST_NOTEBOOK_MODEL.with(|m| m.borrow().clone())
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn test_notebook_model() -> Option<sqlly_ai::catalog::DynLlmProvider> {
    None
}

#[cfg(all(test, not(target_arch = "wasm32")))]
thread_local! {
    /// Tests that need "no AI set up" whatever this machine's own Settings
    /// and local runtimes say (the macOS build agent has both).
    pub(crate) static TEST_NO_AI_SETUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, not(target_arch = "wasm32")))]
fn no_ai_setup_for_test() -> bool {
    TEST_NO_AI_SETUP.with(std::cell::Cell::get)
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn no_ai_setup_for_test() -> bool {
    false
}

impl AppView {
    /// Run the AI cell `cell_id` of the notebook in tab `tab_idx`.
    pub(super) fn run_notebook_ai(
        &mut self,
        tab_idx: usize,
        cell_id: String,
        consented: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(nb) = self.tabs.get(tab_idx).and_then(|t| t.notebook.clone()) else {
            return;
        };
        #[cfg(target_arch = "wasm32")]
        {
            let _ = consented;
            nb.update(cx, |nb, cx| {
                nb.ai_refused(
                    &cell_id,
                    crate::localization::text("notebook.ai.host.unavailable"),
                    cx,
                )
            });
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.start_notebook_ai(nb, tab_idx, cell_id, consented, cx);
    }

    /// Each connection slot's read-only profile: declared slots by their
    /// profile id, and the tab's connection as the current slot.
    #[cfg(not(target_arch = "wasm32"))]
    fn notebook_ai_profiles(
        &self,
        tab_idx: usize,
        doc: &sqlly_services::notebook::NotebookDoc,
        cx: &App,
    ) -> Vec<(String, ConnectionProfile)> {
        use crate::notebook::connection::CellRunTarget;
        let target = |profile_id| CellRunTarget {
            profile_id,
            read_only: true,
            timeout_seconds: None,
            local: false,
        };
        let mut out = vec![];
        for slot in &doc.connection_refs {
            let Some(id) = slot
                .profile_id
                .as_deref()
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
            else {
                continue;
            };
            if let Ok(mut profile) = self.notebook_cell_profile(tab_idx, &target(Some(id)), cx) {
                if slot.database.is_some() {
                    profile.database = slot.database.clone();
                }
                out.push((slot.id.clone(), profile));
            }
        }
        if let Ok(profile) = self.notebook_cell_profile(tab_idx, &target(None), cx) {
            out.push((
                sqlly_services::notebook_security::trust::CURRENT_CONNECTION.to_string(),
                profile,
            ));
        }
        out
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_notebook_ai(
        &mut self,
        nb: Entity<NotebookView>,
        tab_idx: usize,
        cell_id: String,
        consented: bool,
        cx: &mut Context<Self>,
    ) {
        use sqlly_ai::catalog::AgentToolHost;
        use sqlly_services::notebook::cells::ai::AgentMode;
        use sqlly_services::notebook_agent::log::{
            ActionLog, ConsentStore, LogEvent, LogPrivacy, decision, run_entries,
        };
        use sqlly_services::notebook_agent::{
            NotebookAgentEvent, NotebookToolHost, Pricing, context, explain, model_location,
            preview, review, run_notebook_agent, system_clock,
        };
        use sqlly_services::notebook_security::authorize::{
            AiShare, AuthContext, Consents, DataPolicy, Decision, ModelLocation, NotebookAction,
        };
        use sqlly_services::notebook_security::{ConnectionFacts, authorize};

        if nb.read(cx).is_ai_running(&cell_id) {
            return;
        }
        let resolved = test_notebook_model()
            .map(NotebookModel::Ready)
            .unwrap_or_else(|| {
                if no_ai_setup_for_test() {
                    return notebook_model(
                        &crate::cloud_ai::AiProfilesConfig::default(),
                        |_| Ok(None),
                        Arc::new(sqlly_ai::catalog::StubLlmProvider::new()),
                    );
                }
                notebook_model(
                    &crate::cloud_ai::load_ai_profiles(),
                    crate::cloud_ai::load_api_key_ref,
                    self.services.catalog.llm_provider(),
                )
            });
        let provider = match resolved {
            NotebookModel::Ready(provider) => provider,
            NotebookModel::Unavailable(message) => {
                nb.update(cx, |nb, cx| nb.ai_refused(&cell_id, message, cx));
                return;
            }
        };
        let provider_id = provider.provider_id().to_string();
        nb.update(cx, |nb, _| nb.set_ai_provider(Some(provider_id.clone())));
        let Some(mut inputs) = nb.read(cx).ai_run_inputs(&cell_id, cx) else {
            return;
        };

        let mut facts = nb.read(cx).ai_connection_facts();
        let mut hosts: BTreeMap<String, Arc<dyn AgentToolHost>> = BTreeMap::new();
        for (slot, profile) in self.notebook_ai_profiles(tab_idx, &inputs.doc, cx) {
            let profile_id = facts
                .get(&slot)
                .map(|f| f.profile_id.clone())
                .unwrap_or_else(|| Some(profile.id.0.to_string()));
            let fact = ConnectionFacts {
                profile_id: profile_id.clone(),
                engine: Some(profile.database_type),
                environment: profile.environment,
                ..ConnectionFacts::default()
            };
            facts.insert(slot.clone(), fact);
            if !inputs.trust.authorizes(&slot, profile_id.as_deref())
                || self.inline_reads_blocked_message(Some(&profile)).is_some()
            {
                continue;
            }
            let services = self.services_for_profile(&profile);
            hosts.insert(
                slot,
                Arc::new(sqlly_services::ServicesToolHost::new(
                    profile,
                    services.metadata.clone(),
                    services.queries.clone(),
                )),
            );
        }
        let ctx = inputs.context(facts.clone());
        match inputs.request.mode {
            AgentMode::Review => {
                let targets: Vec<String> = inputs.request.target.iter().cloned().collect();
                inputs.request.deterministic = review::review(&ctx, &targets);
            }
            AgentMode::ExplainChanges => {
                let compare = inputs
                    .doc
                    .cell(&cell_id)
                    .and_then(|c| sqlly_services::notebook::cells::ai::AiCellSpec::of(c).compare);
                let runs = compare.and_then(|c| nb.update(cx, |nb, _| nb.ai_compare_runs(&c)));
                inputs.request.deterministic = match runs {
                    Some((before, after)) => explain::explain_runs(&ctx, &before, after.as_ref()),
                    None => {
                        let baseline = inputs
                            .previous
                            .as_ref()
                            .map(|r| r.observed.clone())
                            .unwrap_or_default();
                        explain::explain_since(&ctx, &baseline)
                    }
                };
            }
            _ => {}
        }

        let location = model_location(&provider_id);
        let cloud = location == ModelLocation::Cloud;
        // Prices are what the user typed for the active cloud profile.
        let pricing = if cloud {
            crate::cloud_ai::active_pricing(&crate::cloud_ai::load_ai_profiles())
        } else {
            Pricing::default()
        };
        let preview = preview(&ctx, &inputs.request, &provider_id, cloud);
        let notebook_id = ctx.doc.notebook_id.clone();
        // Consent belongs to the trust record, not the id a file declares.
        let consent_scope = ctx.trust.scope.clone();
        let consent_store = ConsentStore::new(crate::notebook::ai::ai_dir().join("consent.json"));
        if consented
            && cloud
            && let Some(scope) = &consent_scope
        {
            if let Err(error) = consent_store.grant(scope, &provider_id) {
                tracing::warn!(%error, "could not record the notebook AI consent");
            }
            crate::notebook::ai::append_log(
                vec![decision(
                    &notebook_id,
                    &cell_id,
                    &inputs.request.run_id,
                    &chrono::Utc::now().to_rfc3339(),
                    LogEvent::ConsentGranted {
                        provider: provider_id.clone(),
                        context_hash: context::context_hash(&preview.items),
                    },
                )],
                cx,
            );
        }
        let consents = Consents {
            cloud_ai: consented
                || consent_scope
                    .as_deref()
                    .is_some_and(|scope| consent_store.granted(scope, &provider_id)),
            sensitive_export: consented,
            ..Consents::default()
        };
        let share = AiShare {
            model: location,
            includes_rows: preview.may_read_rows,
            sensitive_present: context::sensitive_present(&ctx),
        };
        let verdict = authorize(
            &NotebookAction::ShareWithAi(share),
            &AuthContext {
                doc: &ctx.doc,
                trust: &ctx.trust,
                connections: &facts,
                policy: &DataPolicy::default(),
                consents: &consents,
            },
        );
        match verdict {
            Decision::Deny(reason) => {
                let message = crate::localization::message(reason.message());
                nb.update(cx, |nb, cx| nb.ai_refused(&cell_id, message, cx));
                return;
            }
            Decision::NeedsApproval(_) => {
                nb.update(cx, |nb, cx| nb.ai_needs_consent(&cell_id, preview, cx));
                return;
            }
            Decision::Allow { .. } => {}
        }

        let cancel = crate::notebook::ai::new_cancel();
        let request = inputs.request.clone();
        nb.update(cx, |nb, cx| {
            nb.ai_started(&cell_id, cancel.clone(), request.budget.clone(), cx)
        });
        enum Message {
            Progress(NotebookAgentEvent),
            Done(Box<sqlly_services::notebook::cells::ai::AiResult>),
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let ai_cell = cell_id.clone();
        crate::tokio_handle().spawn(async move {
            let host = NotebookToolHost::new(
                ctx,
                request.mode,
                &request.budget,
                pricing,
                hosts,
                system_clock(),
            );
            let started = chrono::Utc::now().to_rfc3339();
            let progress = tx.clone();
            let result = run_notebook_agent(
                &provider,
                &host,
                &request,
                cancel,
                Some(started.clone()),
                move |event| {
                    let _ = progress.send(Message::Progress(event));
                },
            )
            .await;
            let entries = run_entries(
                &notebook_id,
                &ai_cell,
                &started,
                &result,
                &host.snapshot(),
                &preview,
                &request.prompt,
                LogPrivacy::default(),
            );
            if let Err(error) = ActionLog::new(crate::notebook::ai::ai_dir()).append(&entries) {
                tracing::warn!(%error, "could not write the notebook AI action log");
            }
            let _ = tx.send(Message::Done(Box::new(result)));
        });
        let target = nb.downgrade();
        cx.spawn(async move |_this, cx| {
            while let Some(message) = rx.recv().await {
                match message {
                    Message::Progress(event) => {
                        target
                            .update(cx, |nb, cx| nb.ai_progress(&cell_id, event, cx))
                            .ok();
                    }
                    Message::Done(result) => {
                        target
                            .update(cx, |nb, cx| nb.ai_finished(&cell_id, *result, cx))
                            .ok();
                        break;
                    }
                }
            }
        })
        .detach();
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sqlly_ai::catalog::llm::{CompletionRequest, CompletionResponse, LlmError, LlmProvider};
    use sqlly_services::notebook_agent::model_location;
    use sqlly_services::notebook_security::authorize::ModelLocation;

    use super::*;
    use crate::cloud_ai::{AiProfilesConfig, ProviderKind, ProviderProfile};

    /// A cloud-looking model that counts requests and answers by echoing
    /// them, as the stub does.
    struct Echo {
        id: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for Echo {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = request
                .messages
                .iter()
                .map(|m| format!("[{:?}] {}", m.role, m.content).replace("[System]", "[system]"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(CompletionResponse {
                text,
                model: "m".into(),
                provider: self.id.into(),
                meta: serde_json::Value::Null,
            })
        }

        fn provider_id(&self) -> &str {
            self.id
        }
    }

    fn echo(id: &'static str) -> (sqlly_ai::catalog::DynLlmProvider, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Echo {
                id,
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn profiles(kind: ProviderKind, model: &str, key_ref: &str) -> AiProfilesConfig {
        AiProfilesConfig {
            version: 1,
            active_profile_id: Some("p".into()),
            profiles: vec![ProviderProfile {
                id: "p".into(),
                name: "Docs cloud model".into(),
                kind,
                base_url: "https://models.example.invalid/v1".into(),
                model: model.into(),
                api_key_ref: key_ref.into(),
                available_model_ids: vec![],
                context_window: None,
                reasoning: Default::default(),
                thinking_budget: None,
                send_reasoning_effort: false,
                proxy: None,
                price_per_1k_input_usd: None,
                price_per_1k_output_usd: None,
            }],
        }
    }

    fn ready_id(model: NotebookModel) -> String {
        match model {
            NotebookModel::Ready(p) => p.provider_id().to_string(),
            NotebookModel::Unavailable(message) => panic!("unavailable: {message}"),
        }
    }

    #[test]
    fn every_cloud_profile_kind_runs_as_a_cloud_model() {
        let (local, _) = echo("ollama:llama3");
        for kind in [
            ProviderKind::OpenRouter,
            ProviderKind::OpenAi,
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
            ProviderKind::DeepSeek,
            ProviderKind::OpenAiCompatible,
            ProviderKind::AnthropicCompatible,
            ProviderKind::CheaperInference,
        ] {
            let id = ready_id(notebook_model(
                &profiles(kind, "demo-large", "key"),
                |_| Ok(Some("sk-test".into())),
                local.clone(),
            ));
            assert_eq!(model_location(&id), ModelLocation::Cloud, "{kind:?}: {id}");
        }
    }

    #[test]
    fn a_selected_profile_is_used_or_reported_never_swapped_for_another_model() {
        let (local, _) = echo("ollama:llama3");
        // The docs shot's seed: OpenAI-compatible, no key saved.
        let id = ready_id(notebook_model(
            &profiles(ProviderKind::OpenAiCompatible, "demo-large", ""),
            crate::cloud_ai::load_api_key_ref,
            local.clone(),
        ));
        assert_eq!(id, "openai-compatible:demo-large");
        for (config, key) in [
            (
                profiles(ProviderKind::AnthropicCompatible, "m", ""),
                Ok(None),
            ),
            (
                profiles(ProviderKind::OpenRouter, "", "k"),
                Ok(Some("x".into())),
            ),
            (
                profiles(ProviderKind::OpenAi, "m", "k"),
                Err("locked".into()),
            ),
        ] {
            let key: Result<Option<String>, String> = key;
            match notebook_model(&config, move |_| key.clone(), local.clone()) {
                NotebookModel::Unavailable(message) => {
                    assert!(message.contains("Docs cloud model"), "{message}")
                }
                NotebookModel::Ready(p) => panic!("ran on {}", p.provider_id()),
            }
        }
        let none = AiProfilesConfig::default();
        assert_eq!(
            ready_id(notebook_model(&none, |_| Ok(None), local)),
            "ollama:llama3"
        );
        let (stub, _) = echo("stub");
        assert!(matches!(
            notebook_model(&none, |_| Ok(None), stub),
            NotebookModel::Unavailable(_)
        ));
    }

    fn open_app(cx: &mut gpui::TestAppContext) -> (Entity<AppView>, &mut gpui::VisualTestContext) {
        crate::ensure_test_tokio_runtime();
        cx.dispatcher.allow_parking();
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
            let status = crate::update_status::handle(cx);
            status.update(cx, |s, _| {
                s.state = crate::update_status::UpdateCheckState::Checking;
            });
        });
        let (root, vcx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| AppView::new(sqlly_services::Services::new(), window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        let app = vcx.update(|_, cx| root.read(cx).view().clone().downcast::<AppView>().unwrap());
        (app, vcx)
    }

    fn ai_notebook() -> sqlly_services::notebook::NotebookDoc {
        let mut doc = sqlly_services::notebook::NotebookDoc::empty("nb_ai_consent", "AI");
        let mut cell = sqlly_services::notebook::Cell::new(
            "ask",
            sqlly_services::notebook::CellKind::Ai,
            sqlly_services::notebook::ExecutionPolicy::default(),
        );
        cell.source = "Which customers have no contact email?".into();
        doc.cells.push(cell);
        doc
    }

    /// No profile chosen and no local runtime: the app falls back to the
    /// stub, which used to answer with the agent's system prompt.
    #[gpui::test]
    fn with_no_model_set_up_the_cell_says_so_and_runs_nothing(cx: &mut gpui::TestAppContext) {
        let autosave = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(autosave.path().to_path_buf()));
        super::TEST_NO_AI_SETUP.with(|n| n.set(true));
        let (app, cx) = open_app(cx);
        let nb = app.update(cx, |app, cx| {
            let tab = app.new_notebook_tab(ai_notebook(), None, cx);
            app.tabs[tab].notebook.clone().unwrap()
        });
        cx.run_until_parked();
        nb.update(cx, |nb, cx| nb.ai_request_run("ask", false, cx));
        cx.run_until_parked();
        let (consent, notice, result) = nb.read_with(cx, |nb, _| nb.ai_state_for_test("ask"));
        super::TEST_NO_AI_SETUP.with(|n| n.set(false));
        assert!(!consent);
        assert!(result.is_none(), "nothing ran: {result:?}");
        assert_eq!(
            notice.as_deref(),
            Some(
                crate::localization::text(sqlly_services::notebook_agent::NO_MODEL_DETAIL).as_str()
            )
        );
        crate::notebook::autosave::set_test_root(None);
    }

    #[gpui::test]
    fn a_cloud_model_sees_nothing_before_share_and_run(cx: &mut gpui::TestAppContext) {
        let autosave = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(autosave.path().to_path_buf()));
        let (provider, calls) = echo("openai-compatible:demo-large");
        TEST_NOTEBOOK_MODEL.with(|m| *m.borrow_mut() = Some(provider));
        let (app, cx) = open_app(cx);
        let nb = app.update(cx, |app, cx| {
            let tab = app.new_notebook_tab(ai_notebook(), None, cx);
            app.tabs[tab].notebook.clone().unwrap()
        });
        cx.run_until_parked();

        nb.update(cx, |nb, cx| nb.ai_request_run("ask", false, cx));
        cx.run_until_parked();
        let (consent, notice, result) = nb.read_with(cx, |nb, _| nb.ai_state_for_test("ask"));
        assert!(consent, "the sharing preview shows first: {notice:?}");
        assert!(result.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing was sent");

        // Share and run: the reply only echoes the request, which never shows.
        nb.update(cx, |nb, cx| nb.ai_request_run("ask", true, cx));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if nb.read_with(cx, |nb, _| nb.ai_state_for_test("ask").2.is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (_, _, result) = nb.read_with(cx, |nb, _| nb.ai_state_for_test("ask"));
        let (summary, detail) = result.expect("the run finished");
        assert!(calls.load(Ordering::SeqCst) >= 1);
        assert!(!summary.contains("read-only tools") && !summary.contains("[system]"));
        assert_eq!(
            detail.as_deref(),
            Some(sqlly_services::notebook_agent::INSTRUCTIONS_ECHOED_DETAIL)
        );
        TEST_NOTEBOOK_MODEL.with(|m| *m.borrow_mut() = None);
        crate::notebook::autosave::set_test_root(None);
    }
}
