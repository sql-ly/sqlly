use async_trait::async_trait;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_core::{ConnectionKind, StudioError};
use sqlly_core::{ConnectionProfile, DatabaseSummary, StudioResult};
#[cfg(not(target_arch = "wasm32"))]
use sqlly_engines::pg::SqlTransport;

/// Port: connection lifecycle and discovery.
///
/// The default impl routes each call to the native transport for the
/// profile's engine family (TDS, PostgreSQL, MySQL, SQLite), returning native
/// types (`DatabaseSummary`) with no serialization. A different frontend or a
/// remote adapter can supply its own implementation.
#[async_trait]
pub trait ConnectionManager: Send + Sync {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()>;
    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>>;
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct DefaultConnectionManager {
    tds: sqlly_engines::tds::TiberiusTransport,
    pg: sqlly_engines::pg::PostgresTransport,
    mysql: sqlly_engines::mysql::MySqlTransport,
    sqlite: sqlly_engines::sqlite::SqliteTransport,
    libsql: sqlly_engines::libsql::LibSqlTransport,
    duckdb: sqlly_engines::duckdb::DuckDbTransport,
    clickhouse: sqlly_engines::clickhouse::ClickHouseTransport,
    oracle: sqlly_engines::oracle::OracleTransport,
    redis: sqlly_engines::redis::RedisTransport,
}

#[cfg(not(target_arch = "wasm32"))]
impl DefaultConnectionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The native transport for the profile's engine family, or a clear
    /// unsupported error for kinds without a native driver — mirroring the
    /// dispatch the query executor performs.
    fn transport_for(&self, profile: &ConnectionProfile) -> StudioResult<&dyn SqlTransport> {
        match profile.database_type.connection_kind() {
            ConnectionKind::Tds => Ok(&self.tds),
            ConnectionKind::PostgresWire => Ok(&self.pg),
            ConnectionKind::MySqlWire => Ok(&self.mysql),
            ConnectionKind::SqliteFile => Ok(&self.sqlite),
            ConnectionKind::HranaHttp => Ok(&self.libsql),
            ConnectionKind::DuckDbFile => Ok(&self.duckdb),
            ConnectionKind::ClickHouseHttp => Ok(&self.clickhouse),
            ConnectionKind::OracleNet => Ok(&self.oracle),
            ConnectionKind::Resp => Ok(&self.redis),
            other => Err(StudioError::CapabilityNotSupported(format!(
                "no driver yet for {} ({other:?} connections)",
                profile.database_type.display_name()
            ))),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl ConnectionManager for DefaultConnectionManager {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        self.transport_for(profile)?.test_connection(profile).await
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        self.transport_for(profile)?.list_databases(profile).await
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_core::{DatabaseType, SecretProvider, SecretRef};

    fn profile_for(db: DatabaseType) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "t",
            ":memory:",
            "u",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = db;
        p
    }

    #[tokio::test]
    async fn sqlite_profiles_route_to_the_sqlite_driver() {
        // SQLite is in-process, so a real end-to-end proof needs no server:
        // an in-memory profile tests fine and lists its `main` database. On
        // the pre-dispatch code this attempted a TDS handshake against the
        // file path and failed.
        let manager = DefaultConnectionManager::new();
        let profile = profile_for(DatabaseType::Sqlite);
        manager.test_connection(&profile).await.unwrap();
        let dbs = manager.list_databases(&profile).await.unwrap();
        assert!(!dbs.is_empty());
    }

    #[tokio::test]
    async fn unsupported_kinds_report_a_clear_capability_error() {
        let manager = DefaultConnectionManager::new();
        let profile = profile_for(DatabaseType::Db2);
        let err = manager.test_connection(&profile).await.unwrap_err();
        assert!(
            matches!(err, StudioError::CapabilityNotSupported(_)),
            "{err}"
        );
    }
}
