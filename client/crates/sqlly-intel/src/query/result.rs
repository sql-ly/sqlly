use serde::{Deserialize, Serialize};
use sqlly_core::event::{CellValue, ResultColumn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub result_sets: Vec<ResultSetMeta>,
    pub messages: Vec<Message>,
    pub total_rows: u64,
    pub elapsed: std::time::Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultSetMeta {
    pub index: usize,
    pub columns: Vec<ResultColumn>,
    pub row_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowBatch {
    pub result_set_index: usize,
    pub rows: Vec<Vec<CellValue>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub text: String,
    pub severity: MessageSeverity,
    pub source: Option<String>,
    pub line_number: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageSeverity {
    Info,
    Warning,
    Error,
}
