//! The notebook agent's tool vocabulary. Structured calls only; every name
//! the model can use is listed here, and each mode offers a fixed subset
//! ([`inventory`]). There is no tool that writes, exports, sends anything
//! over a network or changes a policy, so no text can ask for one.

use serde_json::{Value, json};
use sqlly_ai::catalog::llm::{ToolCall, ToolSpec};

use crate::notebook::cells::ai::{AgentMode, ProposedCell};

pub const LIST_CELLS: &str = "list_cells";
pub const READ_CELL: &str = "read_cell";
pub const READ_OUTPUT_SUMMARY: &str = "read_output_summary";
pub const LIST_TABLES: &str = "list_tables";
pub const GET_COLUMNS: &str = "get_columns";
pub const GET_SAMPLE_ROWS: &str = "get_sample_rows";
pub const RUN_READONLY_QUERY: &str = "run_readonly_query";
pub const PROPOSE_CELLS: &str = "propose_cells";
pub const PROPOSE_EDIT: &str = "propose_edit";
pub const PROPOSE_RUN: &str = "propose_run";

/// Every tool, in the order they are offered.
pub const ALL_TOOLS: [&str; 10] = [
    LIST_CELLS,
    READ_CELL,
    READ_OUTPUT_SUMMARY,
    LIST_TABLES,
    GET_COLUMNS,
    GET_SAMPLE_ROWS,
    RUN_READONLY_QUERY,
    PROPOSE_CELLS,
    PROPOSE_EDIT,
    PROPOSE_RUN,
];

/// Most sample rows one `read_output_summary` returns.
pub const MAX_SUMMARY_SAMPLE_ROWS: usize = 20;
/// Most cells one `propose_cells` call may insert.
pub const MAX_PROPOSED_CELLS: usize = 12;
/// Cell kinds the agent may propose. Each spec is checked against its
/// kind's typed shape (`notebook::cells::*`) before it is shown.
pub const PROPOSABLE_KINDS: [&str; 14] = [
    "sql",
    "markdown",
    "parameter",
    "chart",
    "table",
    "pivot",
    "kpi",
    "scalar",
    "transform",
    "profile",
    "assertion",
    "diff",
    "plan",
    "entity_detail",
];

/// The tools a mode offers. Plan, Build and Explain changes never touch a
/// connection's rows; only Assist, Investigate and Review run queries.
pub fn inventory(mode: AgentMode) -> Vec<&'static str> {
    let notebook = [LIST_CELLS, READ_CELL, READ_OUTPUT_SUMMARY];
    let schema = [LIST_TABLES, GET_COLUMNS];
    let rows = [GET_SAMPLE_ROWS, RUN_READONLY_QUERY];
    let mut tools: Vec<&'static str> = notebook.to_vec();
    match mode {
        AgentMode::Assist => {
            tools.extend(schema);
            tools.extend(rows);
            tools.extend([PROPOSE_CELLS, PROPOSE_EDIT]);
        }
        AgentMode::Plan => tools.extend(schema),
        AgentMode::Build => {
            tools.extend(schema);
            tools.extend([PROPOSE_CELLS, PROPOSE_EDIT, PROPOSE_RUN]);
        }
        AgentMode::Investigate => {
            tools.extend(schema);
            tools.extend(rows);
            tools.extend([PROPOSE_CELLS, PROPOSE_RUN]);
        }
        AgentMode::Review => {
            tools.extend(schema);
            tools.extend(rows);
            tools.push(PROPOSE_EDIT);
        }
        AgentMode::ExplainChanges => {}
    }
    tools
}

/// Tools that only read and may run concurrently in one turn.
pub fn is_read_only(name: &str) -> bool {
    matches!(
        name,
        LIST_CELLS
            | READ_CELL
            | READ_OUTPUT_SUMMARY
            | LIST_TABLES
            | GET_COLUMNS
            | GET_SAMPLE_ROWS
            | RUN_READONLY_QUERY
    )
}

/// Tools that read rows (not just names), for the privacy preview.
pub fn reads_rows(name: &str) -> bool {
    matches!(
        name,
        READ_OUTPUT_SUMMARY | GET_SAMPLE_ROWS | RUN_READONLY_QUERY
    )
}

fn connection_property() -> Value {
    json!({"type": "string", "description": "Connection slot id from list_cells; omit for the notebook default"})
}

fn spec_for(name: &str) -> ToolSpec {
    let (description, schema) = match name {
        LIST_CELLS => (
            "List the notebook's cells: id, name, kind, state, connection and whether a result exists.",
            json!({"type": "object", "properties": {}}),
        ),
        READ_CELL => (
            "Read one cell: its source, settings, connection and current state (cell id or name).",
            json!({"type": "object", "properties": {"cell": {"type": "string"}}, "required": ["cell"]}),
        ),
        READ_OUTPUT_SUMMARY => (
            "Summarize a cell's last result: columns, row count, completeness, run id, a column profile and up to 20 sample rows.",
            json!({"type": "object", "properties": {
                "cell": {"type": "string"},
                "sample_rows": {"type": "integer", "description": "0..=20"}
            }, "required": ["cell"]}),
        ),
        LIST_TABLES => (
            "List tables on a notebook connection, optionally in one schema.",
            json!({"type": "object", "properties": {
                "connection": connection_property(),
                "schema": {"type": "string"}
            }}),
        ),
        GET_COLUMNS => (
            "List a table's columns on a notebook connection.",
            json!({"type": "object", "properties": {
                "connection": connection_property(),
                "table": {"type": "string"}
            }, "required": ["table"]}),
        ),
        GET_SAMPLE_ROWS => (
            "Read a few rows of a table (counts against the query and row budget).",
            json!({"type": "object", "properties": {
                "connection": connection_property(),
                "table": {"type": "string"},
                "limit": {"type": "integer"}
            }, "required": ["table"]}),
        ),
        RUN_READONLY_QUERY => (
            "Run one read-only SELECT on a notebook connection (counts against the query, row and time budget). Returns a query id to cite as evidence.",
            json!({"type": "object", "properties": {
                "connection": connection_property(),
                "sql": {"type": "string", "description": "SELECT statement only"},
                "limit": {"type": "integer"}
            }, "required": ["sql"]}),
        ),
        PROPOSE_CELLS => (
            "Propose new cells (sql, markdown, parameter, chart, table, pivot, kpi, scalar, transform, profile, assertion, diff, plan, entity_detail). Non-SQL cells name their input cell in spec.input. Chart spec: {\"input\": \"<cell>\", \"chart\": {\"kind\": \"bar\", \"x\": {\"column\": \"c\"}, \"y\": [{\"column\": \"m\"}]}}. Assertion spec: {\"input\": \"<cell>\", \"assertions\": [{\"kind\": \"unique\", \"columns\": [\"id\"]}]}. Profile spec: {\"input\": \"<cell>\"}. Plan spec (estimated plan of a SQL cell): {\"input\": \"<cell>\"}. Entity detail spec: {\"source\": {\"cell\": \"<cell>\", \"row\": 0}, \"table\": {\"schema\": \"dbo\", \"name\": \"Customers\"}, \"relationships\": [{\"direction\": \"incoming\", \"table\": \"Orders\", \"columns\": [[\"CustomerID\", \"CustomerID\"]]}]}. Nothing changes until the user accepts each one.",
            json!({"type": "object", "properties": {
                "after": {"type": "string", "description": "Insert after this cell id; omit for after the AI cell"},
                "rationale": {"type": "string"},
                "cells": {"type": "array", "items": {"type": "object", "properties": {
                    "kind": {"type": "string"},
                    "name": {"type": "string"},
                    "source": {"type": "string"},
                    "spec": {"type": "object"},
                    "connection": {"type": "string"}
                }, "required": ["kind"]}}
            }, "required": ["cells"]}),
        ),
        PROPOSE_EDIT => (
            "Propose a change to one cell's source or settings. Nothing changes until the user accepts it.",
            json!({"type": "object", "properties": {
                "cell": {"type": "string"},
                "source": {"type": "string"},
                "spec": {"type": "object"},
                "rationale": {"type": "string"}
            }, "required": ["cell"]}),
        ),
        PROPOSE_RUN => (
            "Propose running read-only notebook cells. They run only after the user approves.",
            json!({"type": "object", "properties": {
                "cells": {"type": "array", "items": {"type": "string"}},
                "rationale": {"type": "string"}
            }, "required": ["cells"]}),
        ),
        _ => ("", json!({"type": "object"})),
    };
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: schema,
    }
}

/// The specs offered for `mode`.
pub fn specs(mode: AgentMode) -> Vec<ToolSpec> {
    inventory(mode).into_iter().map(spec_for).collect()
}

/// A parsed, typed call.
#[derive(Debug, Clone, PartialEq)]
pub enum NotebookTool {
    ListCells,
    ReadCell {
        cell: String,
    },
    ReadOutputSummary {
        cell: String,
        sample_rows: usize,
    },
    ListTables {
        connection: Option<String>,
        schema: Option<String>,
    },
    GetColumns {
        connection: Option<String>,
        table: String,
    },
    GetSampleRows {
        connection: Option<String>,
        table: String,
        limit: usize,
    },
    RunReadonlyQuery {
        connection: Option<String>,
        sql: String,
        limit: usize,
    },
    ProposeCells {
        after: Option<String>,
        cells: Vec<ProposedCell>,
        rationale: String,
    },
    ProposeEdit {
        cell: String,
        source: Option<String>,
        spec: Option<Value>,
        rationale: String,
    },
    ProposeRun {
        cells: Vec<String>,
        rationale: String,
    },
}

/// Why a call could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnknownTool(String),
    Missing(&'static str),
    Invalid(&'static str),
}

impl ParseError {
    /// The text the model receives.
    pub fn to_model_text(&self) -> String {
        match self {
            ParseError::UnknownTool(name) => format!(
                "Unknown tool '{name}'. Only these exist: {}.",
                ALL_TOOLS.join(", ")
            ),
            ParseError::Missing(key) => format!("Missing or empty '{key}' argument."),
            ParseError::Invalid(key) => format!("Invalid '{key}' argument."),
        }
    }
}

fn string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn required(args: &Value, key: &'static str) -> Result<String, ParseError> {
    string(args, key).ok_or(ParseError::Missing(key))
}

fn number(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
        .map(|n| n as usize)
}

fn cell_list(args: &Value) -> Result<Vec<String>, ParseError> {
    let cells: Vec<String> = args
        .get("cells")
        .and_then(Value::as_array)
        .ok_or(ParseError::Missing("cells"))?
        .iter()
        .filter_map(|v| v.as_str().map(str::trim).map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect();
    if cells.is_empty() {
        return Err(ParseError::Missing("cells"));
    }
    Ok(cells)
}

fn proposed_cell(value: &Value) -> Result<ProposedCell, ParseError> {
    let kind = string(value, "kind").ok_or(ParseError::Missing("kind"))?;
    let spec = match value.get("spec") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(_) => return Err(ParseError::Invalid("spec")),
    };
    Ok(ProposedCell {
        kind: kind.to_ascii_lowercase(),
        name: string(value, "name"),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        spec,
        connection_ref: string(value, "connection"),
    })
}

/// Parse a provider call into a typed tool.
pub fn parse(call: &ToolCall) -> Result<NotebookTool, ParseError> {
    let args = &call.arguments;
    let connection = string(args, "connection");
    Ok(match call.name.as_str() {
        LIST_CELLS => NotebookTool::ListCells,
        READ_CELL => NotebookTool::ReadCell {
            cell: required(args, "cell")?,
        },
        READ_OUTPUT_SUMMARY => NotebookTool::ReadOutputSummary {
            cell: required(args, "cell")?,
            sample_rows: number(args, "sample_rows")
                .unwrap_or(10)
                .min(MAX_SUMMARY_SAMPLE_ROWS),
        },
        LIST_TABLES => NotebookTool::ListTables {
            connection,
            schema: string(args, "schema"),
        },
        GET_COLUMNS => NotebookTool::GetColumns {
            connection,
            table: required(args, "table")?,
        },
        GET_SAMPLE_ROWS => NotebookTool::GetSampleRows {
            connection,
            table: required(args, "table")?,
            limit: number(args, "limit").unwrap_or(sqlly_ai::catalog::MAX_SAMPLE_ROWS),
        },
        RUN_READONLY_QUERY => NotebookTool::RunReadonlyQuery {
            connection,
            sql: required(args, "sql")?,
            limit: number(args, "limit").unwrap_or(sqlly_ai::catalog::MAX_QUERY_ROWS),
        },
        PROPOSE_CELLS => {
            let items = args
                .get("cells")
                .and_then(Value::as_array)
                .ok_or(ParseError::Missing("cells"))?;
            if items.is_empty() {
                return Err(ParseError::Missing("cells"));
            }
            if items.len() > MAX_PROPOSED_CELLS {
                return Err(ParseError::Invalid("cells"));
            }
            NotebookTool::ProposeCells {
                after: string(args, "after"),
                cells: items.iter().map(proposed_cell).collect::<Result<_, _>>()?,
                rationale: string(args, "rationale").unwrap_or_default(),
            }
        }
        PROPOSE_EDIT => {
            let spec = match args.get("spec") {
                None | Some(Value::Null) => None,
                Some(Value::Object(map)) => Some(Value::Object(map.clone())),
                Some(_) => return Err(ParseError::Invalid("spec")),
            };
            let source = args
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_string);
            if source.is_none() && spec.is_none() {
                return Err(ParseError::Missing("source"));
            }
            NotebookTool::ProposeEdit {
                cell: required(args, "cell")?,
                source,
                spec,
                rationale: string(args, "rationale").unwrap_or_default(),
            }
        }
        PROPOSE_RUN => NotebookTool::ProposeRun {
            cells: cell_list(args)?,
            rationale: string(args, "rationale").unwrap_or_default(),
        },
        other => return Err(ParseError::UnknownTool(other.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments,
        }
    }

    #[test]
    fn modes_offer_fixed_inventories_without_write_or_network_tools() {
        for mode in AgentMode::ALL {
            let tools = inventory(mode);
            assert!(tools.iter().all(|t| ALL_TOOLS.contains(t)));
            for name in &tools {
                let spec = spec_for(name);
                assert!(!spec.description.is_empty(), "{name}");
                assert_eq!(&spec.name, name);
            }
            let executes = tools.contains(&RUN_READONLY_QUERY);
            assert_eq!(executes, mode.executes(), "{mode:?}");
        }
        assert!(
            !inventory(AgentMode::Plan)
                .iter()
                .any(|t| reads_rows(t) && *t != READ_OUTPUT_SUMMARY)
        );
        for name in ALL_TOOLS {
            for banned in [
                "write", "export", "send", "http", "delete", "update", "grant", "exec",
            ] {
                assert!(!name.contains(banned), "{name}");
            }
        }
    }

    #[test]
    fn calls_parse_with_clamps_and_reject_unknown_tools() {
        assert_eq!(
            parse(&call(
                READ_OUTPUT_SUMMARY,
                json!({"cell": "orders", "sample_rows": 500})
            )),
            Ok(NotebookTool::ReadOutputSummary {
                cell: "orders".into(),
                sample_rows: MAX_SUMMARY_SAMPLE_ROWS
            })
        );
        assert_eq!(
            parse(&call(
                "export_results",
                json!({"to": "https://evil.example"})
            )),
            Err(ParseError::UnknownTool("export_results".into()))
        );
        assert_eq!(
            parse(&call(RUN_READONLY_QUERY, json!({"sql": "  "}))),
            Err(ParseError::Missing("sql"))
        );
        assert_eq!(
            parse(&call(PROPOSE_EDIT, json!({"cell": "c1"}))),
            Err(ParseError::Missing("source"))
        );
        let many: Vec<Value> = (0..20).map(|_| json!({"kind": "markdown"})).collect();
        assert_eq!(
            parse(&call(PROPOSE_CELLS, json!({"cells": many}))),
            Err(ParseError::Invalid("cells"))
        );
        let NotebookTool::ProposeCells { cells, .. } = parse(&call(
            PROPOSE_CELLS,
            json!({"cells": [{"kind": "SQL", "source": "SELECT 1", "connection": "conn_1"}]}),
        ))
        .unwrap() else {
            panic!("expected proposal");
        };
        assert_eq!(cells[0].kind, "sql");
        assert_eq!(cells[0].connection_ref.as_deref(), Some("conn_1"));
        assert!(
            ParseError::UnknownTool("x".into())
                .to_model_text()
                .contains(RUN_READONLY_QUERY)
        );
    }
}
