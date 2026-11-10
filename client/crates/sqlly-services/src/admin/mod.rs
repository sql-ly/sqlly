#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};

pub mod activity;
pub mod advanced_tools;
pub mod backup;
pub mod dashboard;
pub mod disk_usage;
pub mod editing;
pub mod error;
pub mod index_usage;
pub mod logs;
pub mod long_tail;
pub mod oracle_dependencies;
pub mod restore;
pub mod schema_plan;
pub mod security_sql;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupDatabaseCommand {
    pub database: String,
    pub destination: String,
    pub compression: bool,
    pub checksum: bool,
}
