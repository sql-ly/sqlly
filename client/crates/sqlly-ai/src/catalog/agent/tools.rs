//! Built-in read-only agent tools and dispatch.

use async_trait::async_trait;

use crate::catalog::llm::{ToolCall, ToolResult, ToolSpec};

pub const MAX_SAMPLE_ROWS: usize = 20;
pub const MAX_QUERY_ROWS: usize = 100;
pub const MAX_TOOL_RESULT_CHARS: usize = 8_000;
pub const MAX_CELL_CHARS: usize = 200;

/// A parsed, validated tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTool {
    ListTables { schema: Option<String> },
    GetColumns { table: String },
    GetSampleRows { table: String, limit: usize },
    RunReadonlyQuery { sql: String, limit: usize },
}

/// JSON Schema specs for the four built-in tools.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "list_tables".into(),
            description: "List tables in the connected database. Optionally filter by schema."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Optional schema name" }
                }
            }),
        },
        ToolSpec {
            name: "get_columns".into(),
            description: "List columns for a table (schema.name or name).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "table": { "type": "string", "description": "Table name, optionally schema-qualified" }
                },
                "required": ["table"]
            }),
        },
        ToolSpec {
            name: "get_sample_rows".into(),
            description: format!("Return up to {MAX_SAMPLE_ROWS} sample rows from a table."),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "limit": { "type": "integer", "description": format!("1..={MAX_SAMPLE_ROWS}") }
                },
                "required": ["table"]
            }),
        },
        ToolSpec {
            name: "run_readonly_query".into(),
            description: format!("Run a SELECT query and return up to {MAX_QUERY_ROWS} rows."),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "SELECT statement only" },
                    "limit": { "type": "integer", "description": format!("1..={MAX_QUERY_ROWS}") }
                },
                "required": ["sql"]
            }),
        },
    ]
}

/// Whether a built-in tool may run concurrently with others in the same turn.
pub fn is_read_only(name: &str) -> bool {
    matches!(
        name,
        "list_tables" | "get_columns" | "get_sample_rows" | "run_readonly_query"
    )
}

/// Parse a provider tool call into a typed [`AgentTool`].
pub fn parse_tool_call(call: &ToolCall) -> Result<AgentTool, String> {
    let args = &call.arguments;
    match call.name.as_str() {
        "list_tables" => {
            let schema = args
                .get("schema")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Ok(AgentTool::ListTables { schema })
        }
        "get_columns" => {
            let table = required_string(args, "table")?;
            Ok(AgentTool::GetColumns { table })
        }
        "get_sample_rows" => {
            let table = required_string(args, "table")?;
            let limit = optional_usize(args, "limit").unwrap_or(MAX_SAMPLE_ROWS);
            Ok(AgentTool::GetSampleRows { table, limit })
        }
        "run_readonly_query" => {
            let sql = required_string(args, "sql")?;
            let limit = optional_usize(args, "limit").unwrap_or(MAX_QUERY_ROWS);
            Ok(AgentTool::RunReadonlyQuery { sql, limit })
        }
        other => Err(format!("Unknown tool '{other}'")),
    }
}

fn required_string(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("Missing or empty '{key}' argument"))
}

fn optional_usize(args: &serde_json::Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

/// Char-safe truncation with a notice when content is shortened.
pub fn window_text(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}\n… (truncated, {char_count} chars total)")
}

/// Host for executing agent tools against live services.
#[async_trait]
pub trait AgentToolHost: Send + Sync {
    async fn list_tables(&self, schema: Option<&str>) -> Result<String, String>;
    async fn get_columns(&self, table: &str) -> Result<String, String>;
    async fn get_sample_rows(&self, table: &str, limit: usize) -> Result<String, String>;
    async fn run_readonly_query(&self, sql: &str, limit: usize) -> Result<String, String>;
}

/// Parse, clamp limits, execute, and window a tool call.
pub async fn dispatch(host: &dyn AgentToolHost, call: &ToolCall) -> ToolResult {
    let parsed = match parse_tool_call(call) {
        Ok(tool) => tool,
        Err(message) => {
            return ToolResult {
                call_id: call.id.clone(),
                content: message,
                is_error: true,
            };
        }
    };
    let result = match parsed {
        AgentTool::ListTables { schema } => host.list_tables(schema.as_deref()).await,
        AgentTool::GetColumns { table } => host.get_columns(&table).await,
        AgentTool::GetSampleRows { table, limit } => {
            let limit = limit.clamp(1, MAX_SAMPLE_ROWS);
            host.get_sample_rows(&table, limit).await
        }
        AgentTool::RunReadonlyQuery { sql, limit } => {
            let limit = limit.clamp(1, MAX_QUERY_ROWS);
            host.run_readonly_query(&sql, limit).await
        }
    };
    match result {
        Ok(content) => ToolResult {
            call_id: call.id.clone(),
            content: window_text(&content, MAX_TOOL_RESULT_CHARS),
            is_error: false,
        },
        Err(message) => ToolResult {
            call_id: call.id.clone(),
            content: message,
            is_error: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingHost {
        sample_limit: Mutex<Option<usize>>,
        query_limit: Mutex<Option<usize>>,
        fail: bool,
        long: bool,
    }

    #[async_trait]
    impl AgentToolHost for RecordingHost {
        async fn list_tables(&self, _schema: Option<&str>) -> Result<String, String> {
            if self.fail {
                return Err("host failed".into());
            }
            Ok("t".into())
        }

        async fn get_columns(&self, _table: &str) -> Result<String, String> {
            Ok("c".into())
        }

        async fn get_sample_rows(&self, _table: &str, limit: usize) -> Result<String, String> {
            *self.sample_limit.lock().unwrap() = Some(limit);
            Ok("rows".into())
        }

        async fn run_readonly_query(&self, _sql: &str, limit: usize) -> Result<String, String> {
            *self.query_limit.lock().unwrap() = Some(limit);
            if self.long {
                Ok("x".repeat(MAX_TOOL_RESULT_CHARS + 500))
            } else {
                Ok("ok".into())
            }
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call-0".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_result() {
        let host = RecordingHost {
            sample_limit: Mutex::new(None),
            query_limit: Mutex::new(None),
            fail: false,
            long: false,
        };
        let result = dispatch(&host, &call("nope", serde_json::json!({}))).await;
        assert!(result.is_error);
        assert!(result.content.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn sample_rows_limit_clamped_to_max() {
        let host = RecordingHost {
            sample_limit: Mutex::new(None),
            query_limit: Mutex::new(None),
            fail: false,
            long: false,
        };
        let _ = dispatch(
            &host,
            &call(
                "get_sample_rows",
                serde_json::json!({"table": "t", "limit": 500}),
            ),
        )
        .await;
        assert_eq!(*host.sample_limit.lock().unwrap(), Some(MAX_SAMPLE_ROWS));
    }

    #[tokio::test]
    async fn readonly_query_limit_zero_clamped_to_one() {
        let host = RecordingHost {
            sample_limit: Mutex::new(None),
            query_limit: Mutex::new(None),
            fail: false,
            long: false,
        };
        let _ = dispatch(
            &host,
            &call(
                "run_readonly_query",
                serde_json::json!({"sql": "SELECT 1", "limit": 0}),
            ),
        )
        .await;
        assert_eq!(*host.query_limit.lock().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn oversized_result_is_windowed() {
        let host = RecordingHost {
            sample_limit: Mutex::new(None),
            query_limit: Mutex::new(None),
            fail: false,
            long: true,
        };
        let result = dispatch(
            &host,
            &call("run_readonly_query", serde_json::json!({"sql": "SELECT 1"})),
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("truncated"));
    }

    #[tokio::test]
    async fn host_error_becomes_is_error() {
        let host = RecordingHost {
            sample_limit: Mutex::new(None),
            query_limit: Mutex::new(None),
            fail: true,
            long: false,
        };
        let result = dispatch(&host, &call("list_tables", serde_json::json!({}))).await;
        assert!(result.is_error);
        assert_eq!(result.content, "host failed");
    }
}
