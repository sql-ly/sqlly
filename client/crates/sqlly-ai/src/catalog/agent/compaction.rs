//! Token-budget compaction for agent transcripts.

use crate::catalog::llm::ToolMessage;

const COMPACTED_MARKER: &str = "[Earlier tool activity compacted: ";
const WINDOWED_TOOL_CHARS: usize = 1_000;

/// Rough token estimate: chars / 4.
pub fn estimate_tokens(messages: &[ToolMessage]) -> usize {
    messages
        .iter()
        .map(message_chars)
        .sum::<usize>()
        .div_ceil(4)
}

fn message_chars(msg: &ToolMessage) -> usize {
    match msg {
        ToolMessage::System(s) | ToolMessage::User(s) => s.chars().count(),
        ToolMessage::Assistant { text, tool_calls } => {
            text.chars().count()
                + tool_calls
                    .iter()
                    .map(|c| c.name.chars().count() + c.arguments.to_string().chars().count())
                    .sum::<usize>()
        }
        ToolMessage::ToolResults(results) => {
            results.iter().map(|r| r.content.chars().count()).sum()
        }
    }
}

/// Drop oldest assistant/tool-result pairs until under budget.
pub fn compact(messages: &mut Vec<ToolMessage>, budget_tokens: usize) -> usize {
    if estimate_tokens(messages) <= budget_tokens {
        return 0;
    }
    let mut removed_pairs = 0usize;
    let mut marker_inserted = messages
        .iter()
        .any(|m| matches!(m, ToolMessage::User(s) if s.starts_with(COMPACTED_MARKER)));
    while estimate_tokens(messages) > budget_tokens {
        if let Some(pair_start) = find_oldest_removable_pair(messages) {
            messages.remove(pair_start + 1);
            messages.remove(pair_start);
            removed_pairs += 1;
            if !marker_inserted {
                if let Some(task_idx) = find_task_user_index(messages) {
                    messages.insert(
                        task_idx + 1,
                        ToolMessage::User(format!(
                            "{COMPACTED_MARKER}{removed_pairs} calls removed]"
                        )),
                    );
                    marker_inserted = true;
                }
            } else if let Some(marker_idx) = messages
                .iter()
                .position(|m| matches!(m, ToolMessage::User(s) if s.starts_with(COMPACTED_MARKER)))
            {
                messages[marker_idx] =
                    ToolMessage::User(format!("{COMPACTED_MARKER}{removed_pairs} calls removed]"));
            }
            continue;
        }
        if window_oldest_tool_results(messages) {
            continue;
        }
        break;
    }
    removed_pairs
}

fn find_task_user_index(messages: &[ToolMessage]) -> Option<usize> {
    let mut seen_system = false;
    for (idx, msg) in messages.iter().enumerate() {
        match msg {
            ToolMessage::System(_) => seen_system = true,
            ToolMessage::User(s) if seen_system && !s.starts_with(COMPACTED_MARKER) => {
                return Some(idx);
            }
            _ => {}
        }
    }
    None
}

fn find_oldest_removable_pair(messages: &[ToolMessage]) -> Option<usize> {
    (0..messages.len().saturating_sub(1)).find(|&idx| is_removable_pair(messages, idx))
}

fn is_removable_pair(messages: &[ToolMessage], idx: usize) -> bool {
    if idx == 0 {
        return false;
    }
    if matches!(messages[idx], ToolMessage::System(_)) {
        return false;
    }
    if let Some(task_idx) = find_task_user_index(messages)
        && idx <= task_idx
    {
        return false;
    }
    matches!(
        (&messages[idx], messages.get(idx + 1)),
        (ToolMessage::Assistant { tool_calls, .. }, Some(ToolMessage::ToolResults(_)))
            if !tool_calls.is_empty()
    )
}

fn window_oldest_tool_results(messages: &mut [ToolMessage]) -> bool {
    for msg in messages.iter_mut() {
        if let ToolMessage::ToolResults(results) = msg {
            let total: usize = results.iter().map(|r| r.content.chars().count()).sum();
            if total > WINDOWED_TOOL_CHARS {
                for result in results.iter_mut() {
                    let chars = result.content.chars().count();
                    if chars > WINDOWED_TOOL_CHARS {
                        result.content = result
                            .content
                            .chars()
                            .take(WINDOWED_TOOL_CHARS)
                            .collect::<String>()
                            + "\n… (truncated)";
                    }
                }
                return true;
            }
        }
    }
    false
}

/// Assert pair integrity: every assistant with tool_calls is followed by matching ToolResults.
pub fn assert_pairs_intact(messages: &[ToolMessage]) {
    let mut idx = 0;
    while idx < messages.len() {
        if let ToolMessage::Assistant { tool_calls, .. } = &messages[idx] {
            if tool_calls.is_empty() {
                idx += 1;
                continue;
            }
            let next = messages
                .get(idx + 1)
                .expect("assistant with calls needs results");
            let ToolMessage::ToolResults(results) = next else {
                panic!("assistant with tool_calls must be followed by ToolResults");
            };
            let call_ids: Vec<_> = tool_calls.iter().map(|c| c.id.as_str()).collect();
            let result_ids: Vec<_> = results.iter().map(|r| r.call_id.as_str()).collect();
            assert_eq!(call_ids, result_ids);
            idx += 2;
            continue;
        }
        if matches!(messages[idx], ToolMessage::ToolResults(_)) {
            panic!("orphan ToolResults without preceding assistant");
        }
        idx += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::llm::{ToolCall, ToolResult};

    fn pair(n: usize, content: &str) -> [ToolMessage; 2] {
        let id = format!("call-{n}");
        [
            ToolMessage::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: id.clone(),
                    name: "list_tables".into(),
                    arguments: serde_json::json!({}),
                }],
            },
            ToolMessage::ToolResults(vec![ToolResult {
                call_id: id,
                content: content.repeat(500),
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn compaction_removes_oldest_pairs_and_retains_system_task() {
        let mut messages = vec![
            ToolMessage::System("sys".into()),
            ToolMessage::User("task".into()),
        ];
        for i in 0..12 {
            messages.extend(pair(i, "x"));
        }
        let removed = compact(&mut messages, 800);
        assert!(removed > 0);
        assert_eq!(messages[0], ToolMessage::System("sys".into()));
        assert_eq!(messages[1], ToolMessage::User("task".into()));
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, ToolMessage::User(s) if s.contains("compacted")))
        );
        assert_pairs_intact(&messages);
    }

    #[test]
    fn compaction_marker_inserted_once() {
        let mut messages = vec![
            ToolMessage::System("sys".into()),
            ToolMessage::User("task".into()),
        ];
        for i in 0..12 {
            messages.extend(pair(i, "y"));
        }
        compact(&mut messages, 400);
        let markers = messages
            .iter()
            .filter(|m| matches!(m, ToolMessage::User(s) if s.starts_with(COMPACTED_MARKER)))
            .count();
        assert_eq!(markers, 1);
    }

    #[test]
    fn under_budget_returns_zero() {
        let mut messages = vec![
            ToolMessage::System("sys".into()),
            ToolMessage::User("task".into()),
        ];
        assert_eq!(compact(&mut messages, 100_000), 0);
    }

    #[test]
    fn pair_integrity_holds_after_compaction() {
        let mut messages = vec![
            ToolMessage::System("sys".into()),
            ToolMessage::User("task".into()),
        ];
        for i in 0..6 {
            messages.extend(pair(i, "z"));
        }
        compact(&mut messages, 3_000);
        assert_pairs_intact(&messages);
    }
}
