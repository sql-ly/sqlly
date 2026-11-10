use super::{CliAgentEvent, MAX_EVENT_BYTES, cap_utf8};

pub fn parse_line(line: &str) -> Vec<CliAgentEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return vec![CliAgentEvent::Ignored],
    };
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "thread.started" => {
            let session_id = value
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            vec![CliAgentEvent::Started {
                session_id,
                mcp_ok: None,
            }]
        }
        "turn.started" => {
            let n = value.get("turn").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            vec![CliAgentEvent::Turn { n }]
        }
        "item.started" => parse_item(&value, false),
        "item.completed" => parse_item(&value, true),
        "error" => {
            let text = value
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| value.get("error").and_then(|v| v.as_str()))
                .unwrap_or("unknown error")
                .to_string();
            vec![CliAgentEvent::Final {
                text,
                is_error: true,
            }]
        }
        _ => vec![CliAgentEvent::Ignored],
    }
}

fn parse_item(value: &serde_json::Value, completed: bool) -> Vec<CliAgentEvent> {
    let Some(item) = value.get("item") else {
        return vec![CliAgentEvent::Ignored];
    };
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match item_type {
        "agent_message" if completed => {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                vec![CliAgentEvent::AssistantText(cap_utf8(
                    text,
                    MAX_EVENT_BYTES,
                ))]
            } else {
                vec![CliAgentEvent::Ignored]
            }
        }
        "mcp_tool_call" if !completed => {
            let id = item.get("id").and_then(|v| v.as_str()).map(str::to_string);
            let server = item
                .get("server")
                .and_then(|v| v.as_str())
                .unwrap_or("sqlly");
            let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("tool");
            let name = format!("{server}__{tool}");
            let arguments_json = item
                .get("arguments")
                .map(|value| {
                    if value.is_string() {
                        value.as_str().unwrap_or("").to_string()
                    } else {
                        serde_json::to_string(value).unwrap_or_default()
                    }
                })
                .unwrap_or_else(|| "{}".to_string());
            vec![CliAgentEvent::ToolCall {
                id,
                name,
                arguments_json,
            }]
        }
        "mcp_tool_call" if completed => {
            let id = item.get("id").and_then(|v| v.as_str()).map(str::to_string);
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let is_error = status != "completed";
            let content = item
                .get("result")
                .or_else(|| item.get("error"))
                .map(|value| {
                    if value.is_string() {
                        value.as_str().unwrap_or("").to_string()
                    } else {
                        serde_json::to_string(value).unwrap_or_default()
                    }
                })
                .unwrap_or_default();
            vec![CliAgentEvent::ToolResult {
                id,
                is_error,
                content: cap_utf8(&content, MAX_EVENT_BYTES),
            }]
        }
        "command_execution" if completed => {
            let command = item
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("(shell)")
                .to_string();
            let output = item
                .get("output")
                .or_else(|| item.get("aggregated_output"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            vec![
                CliAgentEvent::ToolCall {
                    id: None,
                    name: "shell".to_string(),
                    arguments_json: serde_json::json!({ "command": command }).to_string(),
                },
                CliAgentEvent::ToolResult {
                    id: None,
                    is_error: item
                        .get("exit_code")
                        .and_then(|v| v.as_i64())
                        .is_some_and(|code| code != 0),
                    content: cap_utf8(&output, MAX_EVENT_BYTES),
                },
            ]
        }
        _ => vec![CliAgentEvent::Ignored],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::agent_cli::CliAgentEvent;

    #[test]
    fn codex_fixture_parses_to_expected_events() {
        let fixture = include_str!("fixtures/codex_basic.jsonl");
        let mut events = Vec::new();
        for line in fixture.lines() {
            events.extend(parse_line(line));
        }
        assert!(matches!(
            events.first(),
            Some(CliAgentEvent::Started { .. })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            CliAgentEvent::ToolCall { name, .. } if name.contains("describe_table")
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CliAgentEvent::ToolResult { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CliAgentEvent::AssistantText(_)))
        );
    }
}
