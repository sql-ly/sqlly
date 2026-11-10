use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServerCapability {
    SqlAuth,
    WindowsAuth,
    EntraId,
    FullTextSearch,
    ChangeDataCapture,
    ChangeTracking,
    Replication,
    SqlAgent,
    DatabaseMail,
    FileStream,
    FileTable,
    InMemoryOLTP,
    AlwaysOn,
    PolyBase,
    StretchDatabase,
    TemporalTables,
    AlwaysEncrypted,
    RowLevelSecurity,
    DynamicDataMasking,
    QueryStore,
    ExtendedEvents,
    Ledgers,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ServerCapabilities: u64 {
        const SQL_AUTH = 1 << 0;
        const WINDOWS_AUTH = 1 << 1;
        const ENTRA_ID = 1 << 2;
        const FULL_TEXT_SEARCH = 1 << 3;
        const CHANGE_DATA_CAPTURE = 1 << 4;
        const CHANGE_TRACKING = 1 << 5;
        const REPLICATION = 1 << 6;
        const SQL_AGENT = 1 << 7;
        const DATABASE_MAIL = 1 << 8;
        const FILESTREAM = 1 << 9;
        const FILE_TABLE = 1 << 10;
        const IN_MEMORY_OLTP = 1 << 11;
        const ALWAYS_ON = 1 << 12;
        const POLYBASE = 1 << 13;
        const STRETCH_DB = 1 << 14;
        const TEMPORAL_TABLES = 1 << 15;
        const ALWAYS_ENCRYPTED = 1 << 16;
        const ROW_LEVEL_SECURITY = 1 << 17;
        const DYNAMIC_DATA_MASKING = 1 << 18;
        const QUERY_STORE = 1 << 19;
        const EXTENDED_EVENTS = 1 << 20;
        const LEDGERS = 1 << 21;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilityProfile {
    pub server_version: String,
    pub server_edition: String,
    pub engine_edition: EngineEdition,
    pub product_version: String,
    pub product_level: String,
    pub is_azure_sql: bool,
    pub is_azure_mi: bool,
    pub is_linux: bool,
    pub capabilities: ServerCapabilities,
}

impl ServerCapabilityProfile {
    pub fn from_engine_edition(
        engine_edition: EngineEdition,
        server_version: impl Into<String>,
        server_edition: impl Into<String>,
    ) -> Self {
        let is_azure_sql = engine_edition == EngineEdition::SqlDatabase;
        let is_azure_mi = engine_edition == EngineEdition::SqlManagedInstance;
        let mut capabilities = ServerCapabilities::SQL_AUTH
            | ServerCapabilities::FULL_TEXT_SEARCH
            | ServerCapabilities::CHANGE_TRACKING
            | ServerCapabilities::TEMPORAL_TABLES
            | ServerCapabilities::ALWAYS_ENCRYPTED
            | ServerCapabilities::ROW_LEVEL_SECURITY
            | ServerCapabilities::DYNAMIC_DATA_MASKING
            | ServerCapabilities::QUERY_STORE
            | ServerCapabilities::EXTENDED_EVENTS;

        if is_azure_sql || is_azure_mi {
            capabilities |= ServerCapabilities::ENTRA_ID;
        }

        if is_azure_mi {
            capabilities |= ServerCapabilities::SQL_AGENT
                | ServerCapabilities::DATABASE_MAIL
                | ServerCapabilities::CHANGE_DATA_CAPTURE;
        }

        if !is_azure_sql && !is_azure_mi {
            capabilities |= ServerCapabilities::WINDOWS_AUTH
                | ServerCapabilities::SQL_AGENT
                | ServerCapabilities::DATABASE_MAIL
                | ServerCapabilities::FILESTREAM
                | ServerCapabilities::FILE_TABLE
                | ServerCapabilities::IN_MEMORY_OLTP
                | ServerCapabilities::ALWAYS_ON
                | ServerCapabilities::POLYBASE
                | ServerCapabilities::REPLICATION
                | ServerCapabilities::CHANGE_DATA_CAPTURE;
        }

        Self {
            server_version: server_version.into(),
            server_edition: server_edition.into(),
            engine_edition,
            product_version: String::new(),
            product_level: String::new(),
            is_azure_sql,
            is_azure_mi,
            is_linux: false,
            capabilities,
        }
    }

    pub fn azure_sql_database() -> Self {
        Self::from_engine_edition(
            EngineEdition::SqlDatabase,
            "Azure SQL Database",
            "SQL Database",
        )
    }

    pub fn azure_sql_managed_instance() -> Self {
        Self::from_engine_edition(
            EngineEdition::SqlManagedInstance,
            "Azure SQL Managed Instance",
            "SQL Managed Instance",
        )
    }

    pub fn supports(&self, capability: ServerCapability) -> bool {
        let flag = match capability {
            ServerCapability::SqlAuth => ServerCapabilities::SQL_AUTH,
            ServerCapability::WindowsAuth => ServerCapabilities::WINDOWS_AUTH,
            ServerCapability::EntraId => ServerCapabilities::ENTRA_ID,
            ServerCapability::FullTextSearch => ServerCapabilities::FULL_TEXT_SEARCH,
            ServerCapability::ChangeDataCapture => ServerCapabilities::CHANGE_DATA_CAPTURE,
            ServerCapability::ChangeTracking => ServerCapabilities::CHANGE_TRACKING,
            ServerCapability::Replication => ServerCapabilities::REPLICATION,
            ServerCapability::SqlAgent => ServerCapabilities::SQL_AGENT,
            ServerCapability::DatabaseMail => ServerCapabilities::DATABASE_MAIL,
            ServerCapability::FileStream => ServerCapabilities::FILESTREAM,
            ServerCapability::FileTable => ServerCapabilities::FILE_TABLE,
            ServerCapability::InMemoryOLTP => ServerCapabilities::IN_MEMORY_OLTP,
            ServerCapability::AlwaysOn => ServerCapabilities::ALWAYS_ON,
            ServerCapability::PolyBase => ServerCapabilities::POLYBASE,
            ServerCapability::StretchDatabase => ServerCapabilities::STRETCH_DB,
            ServerCapability::TemporalTables => ServerCapabilities::TEMPORAL_TABLES,
            ServerCapability::AlwaysEncrypted => ServerCapabilities::ALWAYS_ENCRYPTED,
            ServerCapability::RowLevelSecurity => ServerCapabilities::ROW_LEVEL_SECURITY,
            ServerCapability::DynamicDataMasking => ServerCapabilities::DYNAMIC_DATA_MASKING,
            ServerCapability::QueryStore => ServerCapabilities::QUERY_STORE,
            ServerCapability::ExtendedEvents => ServerCapabilities::EXTENDED_EVENTS,
            ServerCapability::Ledgers => ServerCapabilities::LEDGERS,
        };
        self.capabilities.contains(flag)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineEdition {
    Personal = 1,
    Standard = 2,
    Enterprise = 3,
    Express = 4,
    SqlDatabase = 5,
    SqlDataWarehouse = 6,
    SqlManagedInstance = 8,
    SqlStretch = 7,
    Unknown = 0,
}

impl From<i32> for EngineEdition {
    fn from(value: i32) -> Self {
        match value {
            1 => Self::Personal,
            2 => Self::Standard,
            3 => Self::Enterprise,
            4 => Self::Express,
            5 => Self::SqlDatabase,
            6 => Self::SqlDataWarehouse,
            7 => Self::SqlStretch,
            8 => Self::SqlManagedInstance,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSqlErrorHint {
    pub category: AzureSqlErrorCategory,
    pub remediation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AzureSqlErrorCategory {
    Firewall,
    Authentication,
    UnsupportedFeature,
    Connectivity,
}

pub fn azure_sql_error_hint(error_number: i32, message: &str) -> Option<AzureSqlErrorHint> {
    let lower = message.to_lowercase();
    let (category, remediation) = match error_number {
        40615 | 47073 => (
            AzureSqlErrorCategory::Firewall,
            "Allow this client IP address in the Azure SQL server firewall and retry.",
        ),
        18456 | 18452 => (
            AzureSqlErrorCategory::Authentication,
            "Verify the login, Entra tenant, MFA state, and database user mapping.",
        ),
        40514 | 40515 => (
            AzureSqlErrorCategory::UnsupportedFeature,
            "This server-level feature is not supported by Azure SQL Database; use the database-scoped alternative when available.",
        ),
        _ if lower.contains("client with ip address") || lower.contains("firewall") => (
            AzureSqlErrorCategory::Firewall,
            "Allow this client IP address in the Azure SQL server firewall and retry.",
        ),
        _ if lower.contains("login failed") || lower.contains("token") => (
            AzureSqlErrorCategory::Authentication,
            "Verify the login, Entra tenant, MFA state, and database user mapping.",
        ),
        _ if lower.contains("not supported") => (
            AzureSqlErrorCategory::UnsupportedFeature,
            "This feature is not supported on the current Azure SQL capability profile.",
        ),
        _ => return None,
    };

    Some(AzureSqlErrorHint {
        category,
        remediation: remediation.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn azure_sql_capabilities_hide_server_only_features() {
        let profile = ServerCapabilityProfile::azure_sql_database();
        assert!(profile.is_azure_sql);
        assert!(profile.supports(ServerCapability::EntraId));
        assert!(profile.supports(ServerCapability::QueryStore));
        assert!(!profile.supports(ServerCapability::SqlAgent));
        assert!(!profile.supports(ServerCapability::WindowsAuth));
    }

    #[test]
    fn managed_instance_keeps_agent_capability() {
        let profile = ServerCapabilityProfile::azure_sql_managed_instance();
        assert!(profile.is_azure_mi);
        assert!(profile.supports(ServerCapability::SqlAgent));
        assert!(profile.supports(ServerCapability::EntraId));
    }

    #[test]
    fn azure_sql_error_hints_classify_firewall_and_auth() {
        let firewall = azure_sql_error_hint(40615, "blocked by firewall").unwrap();
        assert_eq!(firewall.category, AzureSqlErrorCategory::Firewall);

        let auth = azure_sql_error_hint(0, "Login failed for user with token").unwrap();
        assert_eq!(auth.category, AzureSqlErrorCategory::Authentication);
    }
}
