use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlanWarning {
    MissingIndex(MissingIndex),
    TableScan {
        table: String,
        schema: String,
    },
    ImplicitConversion {
        expression: String,
    },
    KeyLookup {
        table: String,
        index: String,
    },
    RowGoal,
    CardinalityEstimation,
    Spool,
    /// Sort or hash work spilled to tempdb (memory grant too small).
    SpillToTempDb,
    NoJoinPredicate,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingIndex {
    pub database: String,
    pub schema: String,
    pub table: String,
    pub equality_columns: Vec<String>,
    pub inequality_columns: Vec<String>,
    pub included_columns: Vec<String>,
    pub impact: f64,
}
