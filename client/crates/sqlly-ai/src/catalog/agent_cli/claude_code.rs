use super::{CliAgentEvent, MAX_EVENT_BYTES, cap_utf8};

pub fn parse_line(line: &str) -> Result<Vec<CliAgentEvent>, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => return Err(error.to_string()),
    };
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "system" if value.get("subtype").and_then(|v| v.as_str()) == Some("init") => {
            let session_id = value
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mcp_ok = value.get("mcp_servers").map(|servers| {
                servers
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter(|row| row.get("name").and_then(|v| v.as_str()) == Some("sqlly"))
                            .all(|row| {
                                row.get("status").and_then(|v| v.as_str()) == Some("connected")
                            })
                    })
                    .unwrap_or(true)
            });
            Ok(vec![CliAgentEvent::Started { session_id, mcp_ok }])
        }
        "assistant" => {
            let mut events = Vec::new();
            if let Some(content) = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match block_type {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                events.push(CliAgentEvent::AssistantText(cap_utf8(
                                    text,
                                    MAX_EVENT_BYTES,
                                )));
                            }
                        }
                        "tool_use" => {
                            let id = block.get("id").and_then(|v| v.as_str()).map(str::to_string);
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            let input =
                                block.get("input").cloned().unwrap_or(serde_json::json!({}));
                            let arguments_json = serde_json::to_string(&input).unwrap_or_default();
                            events.push(CliAgentEvent::ToolCall {
                                id,
                                name,
                                arguments_json,
                            });
                        }
                        _ => events.push(CliAgentEvent::Ignored),
                    }
                }
            }
            Ok(events)
        }
        "user" => {
            let mut events = Vec::new();
            if let Some(content) = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content {
                    if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let is_error = block
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let content_text = tool_result_content(block);
                    events.push(CliAgentEvent::ToolResult {
                        id,
                        is_error,
                        content: cap_utf8(&content_text, MAX_EVENT_BYTES),
                    });
                }
            }
            Ok(events)
        }
        "result" => {
            let text = value
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let is_error = value
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(vec![CliAgentEvent::Final {
                text: cap_utf8(&text, MAX_EVENT_BYTES),
                is_error,
            }])
        }
        _ => Ok(vec![CliAgentEvent::Ignored]),
    }
}

fn tool_result_content(block: &serde_json::Value) -> String {
    if let Some(text) = block.get("content").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    block
        .get("content")
        .map(|value| serde_json::to_string(value).unwrap_or_default())
        .unwrap_or_default()
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use crate::catalog::agent_cli::CliAgentEvent;

    #[test]
    fn claude_fixture_parses_to_expected_events() {
        let fixture = include_str!("fixtures/claude_code_basic.jsonl");
        let mut events = Vec::new();
        for line in fixture.lines() {
            events.extend(parse_line(line).unwrap());
        }
        assert!(matches!(
            events.first(),
            Some(CliAgentEvent::Started {
                mcp_ok: Some(true),
                ..
            })
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
        assert!(matches!(
            events.last(),
            Some(CliAgentEvent::Final {
                is_error: false,
                ..
            })
        ));
    }

    #[test]
    fn claude_mcp_failure_is_detected_from_init() {
        let line = include_str!("fixtures/claude_code_mcp_failed.jsonl")
            .lines()
            .next()
            .unwrap();
        let events = parse_line(line).unwrap();
        assert_eq!(
            events,
            vec![CliAgentEvent::Started {
                session_id: Some("sess-fail".to_string()),
                mcp_ok: Some(false),
            }]
        );
    }

    #[test]
    fn garbage_lines_are_ignored_not_panics() {
        assert!(parse_line("not json at all").is_err());
        assert_eq!(
            parse_line(r#"{"type":"unknown"}"#).unwrap(),
            vec![CliAgentEvent::Ignored]
        );
    }

    #[test]
    fn oversized_event_is_truncated_to_cap() {
        let big = "x".repeat(40 * 1024);
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":{}}}]}}}}"#,
            serde_json::to_string(&big).unwrap()
        );
        let events = parse_line(&line).unwrap();
        match &events[0] {
            CliAgentEvent::AssistantText(text) => {
                assert!(text.ends_with("… (truncated)"));
                assert!(text.len() <= super::MAX_EVENT_BYTES + "… (truncated)".len());
            }
            other => panic!("expected AssistantText, got {other:?}"),
        }
    }
}
