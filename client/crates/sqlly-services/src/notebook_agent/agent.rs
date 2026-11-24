//! The bounded notebook agent loop. It reuses sqlly-ai's provider
//! abstraction (native tool calls, or the text protocol for models without
//! them), its transcript compaction and its parallel read-only dispatch,
//! with the notebook tool host and the budget meter in place of the four
//! schema tools. Each mode ends in an [`AiResult`]: the plan, verified
//! findings, the proposals for the user, the queries that ran and what the
//! budget used.

use std::sync::atomic::Ordering;

use futures::future::join_all;
use sqlly_ai::catalog::agent::compaction::{compact, estimate_tokens};
use sqlly_ai::catalog::agent::run::{CancelFlag, MAX_PARALLEL_TOOL_CALLS};
use sqlly_ai::catalog::llm::{
    DynLlmProvider, LlmError, STUB_LLM_PROVIDER_ID, ToolCall, ToolCompletionRequest, ToolMessage,
    ToolResult, ToolSpec,
};

use super::findings::{parse_answer, verify};
use super::host::{HostRecords, NotebookToolHost};
use super::prompts::{AgentRequest, first_messages};
use super::review::ambiguous_term_findings;
use super::tools;
use crate::notebook::cells::ai::{
    AgentMode, AiResult, BudgetKind, ModelLocationTag, ProposalStatus, RunStatus, TEMPLATE_VERSION,
};
use crate::notebook_security::authorize::ModelLocation;

/// Transcript size kept per request, under the run's token budget.
const TRANSCRIPT_TOKENS: usize = 24_000;

/// Progress the UI shows while a run is going.
#[derive(Debug, Clone, PartialEq)]
pub enum NotebookAgentEvent {
    Turn { n: u32, max: u32 },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Usage(crate::notebook::cells::ai::BudgetUsage),
    Compacted { removed_pairs: usize },
}

/// Whether a provider id names a model that runs on this machine: only the
/// local runtimes' own families (`ollama:…`, `mlx:…`, the `stub`) are local.
/// Everything else, including an OpenAI- or Anthropic-compatible endpoint
/// and any id this build doesn't know, is cloud and needs consent.
pub fn model_location(provider_id: &str) -> ModelLocation {
    let family = provider_id
        .split_once(':')
        .map_or(provider_id, |(family, _)| family);
    if matches!(family, "ollama" | "mlx" | STUB_LLM_PROVIDER_ID) {
        ModelLocation::Local
    } else {
        ModelLocation::Cloud
    }
}

/// The built-in stub answers by echoing its request; it is no model.
pub fn is_stub_provider(provider_id: &str) -> bool {
    provider_id == STUB_LLM_PROVIDER_ID
}

/// Run detail (a catalog key) when no model is set up.
pub const NO_MODEL_DETAIL: &str = "notebook.ai.host.no_model";
/// Run detail (a catalog key) when a reply or a provider error repeats the
/// agent's own instructions or tool list, which are never shown.
pub const INSTRUCTIONS_ECHOED_DETAIL: &str = "notebook.ai.host.instructions_echoed";

/// Whether `text` repeats the agent's instructions or its tool inventory:
/// the text-protocol tool preamble, a `[system]` transcript line, the
/// system prompt's opening line, or a tool's description.
fn echoes_instructions(text: &str, system: &str, tools: &[ToolSpec]) -> bool {
    let opening = system.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    text.contains("You have access to the following read-only tools")
        || text.lines().any(|l| l.trim_start().starts_with("[system]"))
        || (opening.trim().len() >= 24 && text.contains(opening.trim()))
        || tools
            .iter()
            .any(|t| t.description.len() >= 24 && text.contains(t.description.as_str()))
}

fn location_tag(location: ModelLocation) -> ModelLocationTag {
    match location {
        ModelLocation::Local => ModelLocationTag::Local,
        ModelLocation::Cloud => ModelLocationTag::Cloud,
    }
}

struct Ending {
    status: RunStatus,
    stopped_by: Option<BudgetKind>,
    detail: Option<String>,
    final_text: Option<String>,
    model: String,
}

/// Run the agent for one AI cell. Deterministic findings in the request
/// (Review, Explain changes) are kept first and their cells count as read.
pub async fn run_notebook_agent(
    provider: &DynLlmProvider,
    host: &NotebookToolHost,
    request: &AgentRequest,
    cancel: CancelFlag,
    started_at: Option<String>,
    mut on_event: impl FnMut(NotebookAgentEvent),
) -> AiResult {
    for finding in &request.deterministic {
        for cell in finding.evidence_cells() {
            host.note_read(cell);
        }
    }
    if is_stub_provider(provider.provider_id()) {
        let ending = failed(NO_MODEL_DETAIL.to_string());
        return finish(provider, host, request, ending, started_at);
    }
    let (system, task, _) = first_messages(host.context(), request);
    let mut messages = vec![ToolMessage::System(system.clone()), ToolMessage::User(task)];
    let specs = host.specs();
    let budget = host.meter().budget().clone();
    let mut warned_budget = false;

    let ending = loop {
        if cancel.load(Ordering::Relaxed) {
            break Ending {
                status: RunStatus::Stopped,
                stopped_by: None,
                detail: None,
                final_text: None,
                model: String::new(),
            };
        }
        if let Some(kind) = host.meter().hard_stop() {
            break stopped(kind);
        }
        let removed = compact(
            &mut messages,
            TRANSCRIPT_TOKENS.min(budget.max_tokens as usize),
        );
        if removed > 0 {
            on_event(NotebookAgentEvent::Compacted {
                removed_pairs: removed,
            });
        }
        let prompt_tokens = estimate_tokens(&messages) as u64;
        if let Err(kind) = host.meter().begin_turn(prompt_tokens) {
            break stopped(kind);
        }
        let usage = host.meter().usage();
        on_event(NotebookAgentEvent::Turn {
            n: usage.turns,
            max: budget.max_turns,
        });
        let completion = match provider
            .complete_with_tools(ToolCompletionRequest {
                messages: messages.clone(),
                tools: specs.clone(),
                model: None,
                max_tokens: None,
                temperature: None,
            })
            .await
        {
            Ok(completion) => completion,
            Err(LlmError::Other(message)) => {
                break failed(if echoes_instructions(&message, &system, &specs) {
                    INSTRUCTIONS_ECHOED_DETAIL.to_string()
                } else {
                    message
                });
            }
        };
        if echoes_instructions(&completion.text, &system, &specs) {
            break failed(INSTRUCTIONS_ECHOED_DETAIL.to_string());
        }
        let reply_tokens = (completion.text.chars().count()
            + completion
                .tool_calls
                .iter()
                .map(|c| c.name.len() + c.arguments.to_string().len())
                .sum::<usize>())
        .div_ceil(4) as u64;
        host.meter().charge_reply(reply_tokens);
        on_event(NotebookAgentEvent::Usage(host.meter().usage()));
        messages.push(ToolMessage::Assistant {
            text: completion.text.clone(),
            tool_calls: completion.tool_calls.clone(),
        });
        if completion.tool_calls.is_empty() {
            break Ending {
                status: RunStatus::Completed,
                stopped_by: None,
                detail: None,
                final_text: Some(completion.text),
                model: completion.model,
            };
        }
        if warned_budget {
            // The model was told the budget is spent and asked for more.
            let kind = host
                .snapshot()
                .budget_refusals
                .last()
                .copied()
                .unwrap_or(BudgetKind::Queries);
            break stopped(kind);
        }
        let refusals_before = host.snapshot().budget_refusals.len();
        let mut results = Vec::with_capacity(completion.tool_calls.len());
        let calls = &completion.tool_calls;
        let mut index = 0;
        while index < calls.len() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let start = index;
            if tools::is_read_only(&calls[index].name) {
                while index < calls.len() && tools::is_read_only(&calls[index].name) {
                    index += 1;
                }
            } else {
                index += 1;
            }
            for chunk in calls[start..index].chunks(MAX_PARALLEL_TOOL_CALLS) {
                for call in chunk {
                    on_event(NotebookAgentEvent::ToolCall(call.clone()));
                }
                for result in join_all(chunk.iter().map(|call| host.dispatch(call))).await {
                    on_event(NotebookAgentEvent::ToolResult(result.clone()));
                    results.push(result);
                }
            }
        }
        // Calls skipped by a cancel still need a result for the transcript.
        for call in &calls[results.len()..] {
            results.push(ToolResult {
                call_id: call.id.clone(),
                content: "Cancelled.".into(),
                is_error: true,
            });
        }
        messages.push(ToolMessage::ToolResults(results));
        on_event(NotebookAgentEvent::Usage(host.meter().usage()));
        if host.snapshot().budget_refusals.len() > refusals_before {
            warned_budget = true;
            messages.push(ToolMessage::User(
                "The budget for this run is used up. Reply now with your summary and findings, with no tool calls."
                    .into(),
            ));
        }
    };
    finish(provider, host, request, ending, started_at)
}

fn failed(detail: String) -> Ending {
    Ending {
        status: RunStatus::Failed,
        stopped_by: None,
        detail: Some(detail),
        final_text: None,
        model: String::new(),
    }
}

fn stopped(kind: BudgetKind) -> Ending {
    Ending {
        status: RunStatus::Stopped,
        stopped_by: Some(kind),
        detail: None,
        final_text: None,
        model: String::new(),
    }
}

fn finish(
    provider: &DynLlmProvider,
    host: &NotebookToolHost,
    request: &AgentRequest,
    ending: Ending,
    started_at: Option<String>,
) -> AiResult {
    let records: HostRecords = host.snapshot();
    let ctx = host.context();
    let provider_id = provider.provider_id().to_string();
    let mut result = AiResult {
        run_id: request.run_id.clone(),
        mode: request.mode,
        status: ending.status,
        stopped_by: ending.stopped_by,
        detail: ending.detail,
        model_location: Some(location_tag(model_location(&provider_id))),
        provider: provider_id,
        model: ending.model,
        template_version: TEMPLATE_VERSION.to_string(),
        findings: request.deterministic.clone(),
        proposals: records.proposals.clone(),
        queries: records.queries.clone(),
        observed: records.observed.clone(),
        budget: host.meter().budget().clone(),
        usage: host.meter().usage(),
        tools: host.inventory().iter().map(|s| s.to_string()).collect(),
        started_at,
        ..AiResult::default()
    };
    if let Some(text) = ending.final_text {
        let answer = parse_answer(&text);
        result.summary = answer.prose;
        let mut model_findings = verify(
            &answer.findings,
            ctx,
            &records.evidence,
            result.findings.len() + 1,
        );
        result.findings.append(&mut model_findings);
        super::findings::mark_conflicts(&mut result.findings);
        result.clarifications = answer.clarifications;
        if request.mode == AgentMode::Plan {
            result.plan = answer.plan;
        }
        let terms = super::context::ambiguous_terms(&request.prompt, &ctx.doc);
        if !result.clarifications.is_empty() {
            result.status = RunStatus::NeedsClarification;
        } else if result.plan.is_some() {
            result.status = RunStatus::AwaitingApproval;
        } else if !terms.is_empty() && !result.findings.is_empty() {
            let first = result.findings.len() + 1;
            result
                .findings
                .extend(ambiguous_term_findings(&terms, first));
        }
    }
    if !result
        .proposals
        .iter()
        .any(|p| p.status == ProposalStatus::Pending)
        && result.status == RunStatus::Completed
        && result.findings.is_empty()
        && result.summary.is_empty()
    {
        result.status = RunStatus::Failed;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_map_to_local_or_cloud() {
        assert_eq!(model_location("ollama:llama3"), ModelLocation::Local);
        assert_eq!(model_location("mlx:qwen"), ModelLocation::Local);
        assert_eq!(model_location("stub"), ModelLocation::Local);
        for cloud in [
            "claude:claude-sonnet",
            "openrouter:meta/llama",
            "openai:gpt-4o",
            "openai-compatible:demo-large",
            "anthropic:claude-opus",
            "anthropic-compatible:m",
            "gemini:gemini-pro",
            "deepseek:chat",
            "cheaper-inference:m",
            "cli:codex",
            "cli:claude-code",
            "jev",
            // Names that only start like a local runtime are not one.
            "ollama-cloud:m",
            "ollamahub:m",
            "mlx-server:m",
            "localai:m",
            "local-proxy:m",
            "stubby:m",
            "",
        ] {
            assert_eq!(model_location(cloud), ModelLocation::Cloud, "{cloud:?}");
        }
    }

    #[test]
    fn replies_repeating_the_instructions_are_caught() {
        let system = "You are the SQLLY notebook agent. Read the notebook first.\nMore.";
        let tools = vec![ToolSpec {
            name: "read_cell".into(),
            description: "Read one cell's source and output summary.".into(),
            input_schema: serde_json::json!({}),
        }];
        for echo in [
            "[system] You are the SQLLY notebook agent.",
            "You have access to the following read-only tools.\nread_cell({})",
            "Sure. You are the SQLLY notebook agent. Read the notebook first.",
            "read_cell: Read one cell's source and output summary.",
        ] {
            assert!(echoes_instructions(echo, system, &tools), "{echo}");
        }
        assert!(!echoes_instructions(
            "Germany has 522 orders; the freight check passes.",
            system,
            &tools
        ));
    }
}
