use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTablesRequest {
    /// Connection name or id from list_connections.
    pub connection: String,
    pub database: Option<String>,
    pub schema: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeTableRequest {
    pub connection: String,
    pub database: Option<String>,
    /// `schema.table` or bare table name.
    pub table: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SchemaContextRequest {
    pub connection: String,
    pub database: Option<String>,
    /// Approximate token budget, 256..=8000 (default 2000).
    pub budget: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadonlyQueryRequest {
    pub connection: String,
    pub database: Option<String>,
    /// A single SELECT (or WITH … SELECT). Anything else is refused without executing.
    pub sql: String,
    /// Max rows, ≤ 100 (policy may lower it).
    pub limit: Option<usize>,
    /// Max characters per cell, ≤ 2000.
    pub cell_char_limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpenInAppRequest {
    pub connection: String,
    pub database: Option<String>,
    /// Optional SQL to place in the new query tab.
    pub sql: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeWriteRequest {
    pub connection: String,
    pub database: Option<String>,
    /// Exact SQL to propose. It is NOT executed here; SQLLY shows it to the user for confirmation.
    pub sql: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposalStatusRequest {
    #[schemars(with = "String")]
    pub id: uuid::Uuid,
}
