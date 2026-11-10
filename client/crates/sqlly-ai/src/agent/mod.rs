#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};

pub mod error;
pub mod history;
pub mod jobs;
pub mod operator;
pub mod schedule;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentJobSummary {
    pub name: String,
    pub enabled: bool,
}
