//! Bounded read-only agent loop.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::catalog::agent::compaction::compact;
use crate::catalog::agent::tools::{self, AgentToolHost};
use crate::catalog::llm::{
    DynLlmProvider, LlmError, ToolCall, ToolCompletionRequest, ToolMessage, ToolResult,
};
use futures::future::join_all;

pub const MAX_PARALLEL_TOOL_CALLS: usize = 4;

pub type CancelFlag = Arc<AtomicBool>;

/// Configuration for a single agent run.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_turns: usize,
    pub token_budget: usize,
    pub max_tokens_per_reply: Option<usize>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 30,
            token_budget: 24_000,
            max_tokens_per_reply: None,
        }
    }
}

impl AgentConfig {
    pub fn clamped_max_turns(&self) -> usize {
        self.max_turns.clamp(5, 100)
    }
}

/// Events emitted during an agent run for UI rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Turn { n: usize, max: usize },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Compacted { removed_pairs: usize },
    Final(String),
    Stopped(StopReason),
}

/// Why an agent run ended before a final answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    TurnLimit,
    Cancelled,
    ProviderError(String),
}

/// Outcome of an agent run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOutcome {
    pub final_text: Option<String>,
    pub stop: Option<StopReason>,
    pub turns_used: usize,
}

/// Run the bounded read-only tool loop.
pub async fn run_agent(
    provider: &DynLlmProvider,
    host: &dyn AgentToolHost,
    system: String,
    task: String,
    config: AgentConfig,
    cancel: CancelFlag,
    mut on_event: impl FnMut(AgentEvent),
) -> AgentOutcome {
    let max_turns = config.clamped_max_turns();
    let tools = tools::tool_specs();
    let mut messages = vec![ToolMessage::System(system), ToolMessage::User(task)];
    let mut turns_used = 0usize;

    loop {
        if cancel.load(Ordering::Relaxed) {
            on_event(AgentEvent::Stopped(StopReason::Cancelled));
            return AgentOutcome {
                final_text: None,
                stop: Some(StopReason::Cancelled),
                turns_used,
            };
        }

        let removed = compact(&mut messages, config.token_budget);
        if removed > 0 {
            on_event(AgentEvent::Compacted {
                removed_pairs: removed,
            });
        }

        turns_used += 1;
        on_event(AgentEvent::Turn {
            n: turns_used,
            max: max_turns,
        });

        let request = ToolCompletionRequest {
            messages: messages.clone(),
            tools: tools.clone(),
            model: None,
            max_tokens: config.max_tokens_per_reply,
            temperature: None,
        };

        let completion = match provider.complete_with_tools(request).await {
            Ok(c) => c,
            Err(LlmError::Other(message)) => {
                on_event(AgentEvent::Stopped(StopReason::ProviderError(
                    message.clone(),
                )));
                return AgentOutcome {
                    final_text: None,
                    stop: Some(StopReason::ProviderError(message)),
                    turns_used,
                };
            }
        };

        messages.push(ToolMessage::Assistant {
            text: completion.text.clone(),
            tool_calls: completion.tool_calls.clone(),
        });

        if completion.tool_calls.is_empty() {
            on_event(AgentEvent::Final(completion.text.clone()));
            return AgentOutcome {
                final_text: Some(completion.text),
                stop: None,
                turns_used,
            };
        }

        let mut results = Vec::with_capacity(completion.tool_calls.len());
        for (start, end) in tool_call_runs(&completion.tool_calls) {
            if cancel.load(Ordering::Relaxed) {
                on_event(AgentEvent::Stopped(StopReason::Cancelled));
                return AgentOutcome {
                    final_text: None,
                    stop: Some(StopReason::Cancelled),
                    turns_used,
                };
            }

            let run = &completion.tool_calls[start..end];
            for call in run {
                on_event(AgentEvent::ToolCall(call.clone()));
            }

            if run.len() == 1 && !tools::is_read_only(&run[0].name) {
                let result = tools::dispatch(host, &run[0]).await;
                on_event(AgentEvent::ToolResult(result.clone()));
                results.push(result);
                continue;
            }

            for chunk in run.chunks(MAX_PARALLEL_TOOL_CALLS) {
                if cancel.load(Ordering::Relaxed) {
                    on_event(AgentEvent::Stopped(StopReason::Cancelled));
                    return AgentOutcome {
                        final_text: None,
                        stop: Some(StopReason::Cancelled),
                        turns_used,
                    };
                }
                let chunk_results =
                    join_all(chunk.iter().map(|call| tools::dispatch(host, call))).await;
                for result in chunk_results {
                    on_event(AgentEvent::ToolResult(result.clone()));
                    results.push(result);
                }
            }
        }
        messages.push(ToolMessage::ToolResults(results));

        if turns_used >= max_turns {
            on_event(AgentEvent::Stopped(StopReason::TurnLimit));
            return AgentOutcome {
                final_text: None,
                stop: Some(StopReason::TurnLimit),
                turns_used,
            };
        }
    }
}

fn tool_call_runs(calls: &[ToolCall]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut index = 0;
    while index < calls.len() {
        let start = index;
        if tools::is_read_only(&calls[index].name) {
            index += 1;
            while index < calls.len() && tools::is_read_only(&calls[index].name) {
                index += 1;
            }
        } else {
            index += 1;
        }
        runs.push((start, index));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::agent::tools::AgentToolHost;
    use crate::catalog::llm::{LlmProvider, ToolCompletion, ToolCompletionRequest};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct ScriptedToolProvider {
        script: Mutex<Vec<ToolCompletion>>,
    }

    impl ScriptedToolProvider {
        fn new(script: Vec<ToolCompletion>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedToolProvider {
        async fn complete(
            &self,
            _request: crate::catalog::llm::CompletionRequest,
        ) -> Result<crate::catalog::llm::CompletionResponse, LlmError> {
            Err(LlmError::Other("unused".into()))
        }

        async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<ToolCompletion, LlmError> {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Ok(ToolCompletion {
                    text: "done".into(),
                    tool_calls: vec![],
                    model: "test".into(),
                    provider: "test".into(),
                });
            }
            Ok(script.remove(0))
        }
    }

    struct RecordingHost {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AgentToolHost for RecordingHost {
        async fn list_tables(&self, _schema: Option<&str>) -> Result<String, String> {
            self.calls.lock().unwrap().push("list_tables".into());
            Ok("users".into())
        }

        async fn get_columns(&self, _table: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push("get_columns".into());
            Ok("id int".into())
        }

        async fn get_sample_rows(&self, _table: &str, _limit: usize) -> Result<String, String> {
            Ok(String::new())
        }

        async fn run_readonly_query(&self, _sql: &str, _limit: usize) -> Result<String, String> {
            Ok(String::new())
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn dispatches_each_call_and_feeds_results_back() {
        let provider = ScriptedToolProvider::new(vec![
            ToolCompletion {
                text: String::new(),
                tool_calls: vec![
                    call("list_tables"),
                    ToolCall {
                        id: "call-get_columns".into(),
                        name: "get_columns".into(),
                        arguments: serde_json::json!({"table": "users"}),
                    },
                ],
                model: "m".into(),
                provider: "p".into(),
            },
            ToolCompletion {
                text: "final answer".into(),
                tool_calls: vec![],
                model: "m".into(),
                provider: "p".into(),
            },
        ]);
        let host = RecordingHost {
            calls: Mutex::new(Vec::new()),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = Vec::new();
        let provider: DynLlmProvider = provider;
        let outcome = run_agent(
            &provider,
            &host,
            "sys".into(),
            "task".into(),
            AgentConfig::default(),
            cancel,
            |event| events.push(event),
        )
        .await;
        assert_eq!(host.calls.lock().unwrap().len(), 2);
        assert_eq!(outcome.final_text, Some("final answer".into()));
        assert_eq!(outcome.turns_used, 2);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Final(_))));
    }

    struct AlwaysToolProvider;

    #[async_trait]
    impl LlmProvider for AlwaysToolProvider {
        async fn complete(
            &self,
            _request: crate::catalog::llm::CompletionRequest,
        ) -> Result<crate::catalog::llm::CompletionResponse, LlmError> {
            Err(LlmError::Other("unused".into()))
        }

        async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<ToolCompletion, LlmError> {
            Ok(ToolCompletion {
                text: String::new(),
                tool_calls: vec![call("list_tables")],
                model: "m".into(),
                provider: "p".into(),
            })
        }
    }

    #[tokio::test]
    async fn turn_limit_exits_with_stopped() {
        let provider: DynLlmProvider = Arc::new(AlwaysToolProvider);
        let host = RecordingHost {
            calls: Mutex::new(Vec::new()),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = Vec::new();
        let provider: DynLlmProvider = provider;
        let outcome = run_agent(
            &provider,
            &host,
            "sys".into(),
            "task".into(),
            AgentConfig {
                max_turns: 5,
                ..Default::default()
            },
            cancel,
            |event| events.push(event),
        )
        .await;
        assert_eq!(outcome.turns_used, 5);
        assert!(outcome.final_text.is_none());
        assert_eq!(
            events.last(),
            Some(&AgentEvent::Stopped(StopReason::TurnLimit))
        );
    }

    #[tokio::test]
    async fn cancel_flag_stops_before_next_provider_call() {
        let provider = ScriptedToolProvider::new(vec![
            ToolCompletion {
                text: String::new(),
                tool_calls: vec![call("list_tables")],
                model: "m".into(),
                provider: "p".into(),
            },
            ToolCompletion {
                text: "should not run".into(),
                tool_calls: vec![],
                model: "m".into(),
                provider: "p".into(),
            },
        ]);
        let host = RecordingHost {
            calls: Mutex::new(Vec::new()),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let mut provider_calls = 0usize;
        let mut events = Vec::new();
        let provider_for_test: DynLlmProvider = provider;
        let outcome = run_agent(
            &provider_for_test,
            &host,
            "sys".into(),
            "task".into(),
            AgentConfig::default(),
            Arc::clone(&cancel),
            |event| {
                if let AgentEvent::ToolResult(_) = &event {
                    cancel.store(true, Ordering::Relaxed);
                }
                if matches!(event, AgentEvent::Turn { .. }) {
                    provider_calls += 1;
                }
                events.push(event);
            },
        )
        .await;
        assert_eq!(provider_calls, 1);
        assert_eq!(outcome.stop, Some(StopReason::Cancelled));
    }

    struct FailingProvider;

    #[async_trait]
    impl LlmProvider for FailingProvider {
        async fn complete(
            &self,
            _request: crate::catalog::llm::CompletionRequest,
        ) -> Result<crate::catalog::llm::CompletionResponse, LlmError> {
            Err(LlmError::Other("boom".into()))
        }

        async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<ToolCompletion, LlmError> {
            Err(LlmError::Other("boom".into()))
        }
    }

    #[tokio::test]
    async fn provider_error_is_a_stop_not_a_panic() {
        let provider = Arc::new(FailingProvider);
        let host = RecordingHost {
            calls: Mutex::new(Vec::new()),
        };
        let provider: DynLlmProvider = provider;
        let outcome = run_agent(
            &provider,
            &host,
            "sys".into(),
            "task".into(),
            AgentConfig::default(),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        assert!(matches!(outcome.stop, Some(StopReason::ProviderError(_))));
    }

    #[test]
    fn max_turns_is_clamped() {
        assert_eq!(
            AgentConfig {
                max_turns: 1000,
                ..Default::default()
            }
            .clamped_max_turns(),
            100
        );
        assert_eq!(
            AgentConfig {
                max_turns: 0,
                ..Default::default()
            }
            .clamped_max_turns(),
            5
        );
    }

    struct SlowHost {
        calls: Mutex<Vec<(String, std::time::Instant)>>,
        cancel_after_first_batch: Option<Arc<AtomicBool>>,
    }

    #[async_trait]
    impl AgentToolHost for SlowHost {
        async fn list_tables(&self, _schema: Option<&str>) -> Result<String, String> {
            self.record("list_tables").await;
            Ok("users".into())
        }

        async fn get_columns(&self, _table: &str) -> Result<String, String> {
            self.record("get_columns").await;
            Ok("id int".into())
        }

        async fn get_sample_rows(&self, _table: &str, _limit: usize) -> Result<String, String> {
            self.record("get_sample_rows").await;
            Ok(String::new())
        }

        async fn run_readonly_query(&self, _sql: &str, _limit: usize) -> Result<String, String> {
            self.record("run_readonly_query").await;
            Ok(String::new())
        }
    }

    impl SlowHost {
        async fn record(&self, name: &str) {
            let started = {
                let mut calls = self.calls.lock().unwrap();
                calls.push((name.to_string(), std::time::Instant::now()));
                calls.len()
            };
            if let Some(flag) = &self.cancel_after_first_batch
                && started >= MAX_PARALLEL_TOOL_CALLS
            {
                flag.store(true, Ordering::Relaxed);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn read_only_calls_in_one_turn_run_concurrently() {
        let provider = ScriptedToolProvider::new(vec![
            ToolCompletion {
                text: String::new(),
                tool_calls: vec![
                    call("list_tables"),
                    call("get_columns"),
                    call("get_sample_rows"),
                ],
                model: "m".into(),
                provider: "p".into(),
            },
            ToolCompletion {
                text: "done".into(),
                tool_calls: vec![],
                model: "m".into(),
                provider: "p".into(),
            },
        ]);
        let host = SlowHost {
            calls: Mutex::new(Vec::new()),
            cancel_after_first_batch: None,
        };
        let started = std::time::Instant::now();
        let mut events = Vec::new();
        let provider: DynLlmProvider = provider;
        let outcome = run_agent(
            &provider,
            &host,
            "sys".into(),
            "task".into(),
            AgentConfig::default(),
            Arc::new(AtomicBool::new(false)),
            |event| events.push(event),
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_millis(120));
        assert_eq!(outcome.final_text, Some("done".into()));
        let tool_calls = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCall(_)))
            .count();
        let tool_results = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolResult(_)))
            .count();
        assert_eq!(tool_calls, 3);
        assert_eq!(tool_results, 3);
        let first_call = events
            .iter()
            .position(|event| matches!(event, AgentEvent::ToolCall(_)))
            .unwrap();
        let first_result = events
            .iter()
            .position(|event| matches!(event, AgentEvent::ToolResult(_)))
            .unwrap();
        assert!(first_call < first_result);
    }

    #[tokio::test]
    async fn results_keep_provider_order_across_chunks() {
        let provider = ScriptedToolProvider::new(vec![
            ToolCompletion {
                text: String::new(),
                tool_calls: (0..6)
                    .map(|index| ToolCall {
                        id: format!("call-{index}"),
                        name: if index % 2 == 0 {
                            "list_tables".into()
                        } else {
                            "get_columns".into()
                        },
                        arguments: serde_json::json!({}),
                    })
                    .collect(),
                model: "m".into(),
                provider: "p".into(),
            },
            ToolCompletion {
                text: "done".into(),
                tool_calls: vec![],
                model: "m".into(),
                provider: "p".into(),
            },
        ]);
        let host = SlowHost {
            calls: Mutex::new(Vec::new()),
            cancel_after_first_batch: None,
        };
        let mut messages = vec![
            ToolMessage::System("sys".into()),
            ToolMessage::User("task".into()),
        ];
        let provider: DynLlmProvider = provider;
        let completion = provider
            .complete_with_tools(ToolCompletionRequest {
                messages: messages.clone(),
                tools: tools::tool_specs(),
                model: None,
                max_tokens: None,
                temperature: None,
            })
            .await
            .unwrap();
        messages.push(ToolMessage::Assistant {
            text: completion.text.clone(),
            tool_calls: completion.tool_calls.clone(),
        });
        let mut results = Vec::new();
        for (start, end) in tool_call_runs(&completion.tool_calls) {
            let run = &completion.tool_calls[start..end];
            for chunk in run.chunks(MAX_PARALLEL_TOOL_CALLS) {
                let chunk_results =
                    join_all(chunk.iter().map(|call| tools::dispatch(&host, call))).await;
                results.extend(chunk_results);
            }
        }
        messages.push(ToolMessage::ToolResults(results.clone()));
        let ToolMessage::ToolResults(stored) = messages.last().unwrap() else {
            panic!("expected tool results");
        };
        assert_eq!(
            stored
                .iter()
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>(),
            completion
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn unknown_tool_runs_sequentially_and_still_errors() {
        let provider = ScriptedToolProvider::new(vec![
            ToolCompletion {
                text: String::new(),
                tool_calls: vec![
                    call("list_tables"),
                    ToolCall {
                        id: "call-drop".into(),
                        name: "drop_everything".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolCall {
                        id: "call-get_columns".into(),
                        name: "get_columns".into(),
                        arguments: serde_json::json!({"table": "users"}),
                    },
                ],
                model: "m".into(),
                provider: "p".into(),
            },
            ToolCompletion {
                text: "done".into(),
                tool_calls: vec![],
                model: "m".into(),
                provider: "p".into(),
            },
        ]);
        let host = SlowHost {
            calls: Mutex::new(Vec::new()),
            cancel_after_first_batch: None,
        };
        let mut events = Vec::new();
        let provider: DynLlmProvider = provider;
        let outcome = run_agent(
            &provider,
            &host,
            "sys".into(),
            "task".into(),
            AgentConfig::default(),
            Arc::new(AtomicBool::new(false)),
            |event| events.push(event),
        )
        .await;
        assert_eq!(outcome.final_text, Some("done".into()));
        let error_result = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolResult(result) if result.is_error => Some(result.clone()),
                _ => None,
            })
            .expect("missing error result");
        assert!(error_result.content.contains("Unknown tool"));
        let starts = host.calls.lock().unwrap();
        assert_eq!(starts.len(), 2);
        assert!(starts[1].1 >= starts[0].1);
    }

    #[tokio::test]
    async fn cancel_between_batches_stops_run() {
        let provider = ScriptedToolProvider::new(vec![ToolCompletion {
            text: String::new(),
            tool_calls: (0..6)
                .map(|index| ToolCall {
                    id: format!("call-{index}"),
                    name: "list_tables".into(),
                    arguments: serde_json::json!({}),
                })
                .collect(),
            model: "m".into(),
            provider: "p".into(),
        }]);
        let cancel = Arc::new(AtomicBool::new(false));
        let host = SlowHost {
            calls: Mutex::new(Vec::new()),
            cancel_after_first_batch: Some(Arc::clone(&cancel)),
        };
        let provider: DynLlmProvider = provider;
        let outcome = run_agent(
            &provider,
            &host,
            "sys".into(),
            "task".into(),
            AgentConfig::default(),
            cancel,
            |_| {},
        )
        .await;
        assert_eq!(outcome.stop, Some(StopReason::Cancelled));
        assert!(host.calls.lock().unwrap().len() <= MAX_PARALLEL_TOOL_CALLS);
    }
}
