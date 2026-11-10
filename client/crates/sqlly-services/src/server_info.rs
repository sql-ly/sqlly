//! Server facts gathered right after a successful connection test:
//! product, version, charset/collation, identifier case sensitivity,
//! current user/database, TLS, and client-measured latency. One read-only
//! probe per engine family, run through the `QueryExecutor` port so every
//! route (direct, relay, browser) reuses it.

use sqlly_core::{CellValue, ConnectionProfile, StudioError, StudioResult};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

use crate::query::{QueryEvent, QueryExecutor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum IdentifierCase {
    Sensitive,
    Insensitive,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ServerInfo {
    pub product: String,
    pub version: String,
    pub server_charset: Option<String>,
    pub server_collation: Option<String>,
    pub identifier_case: IdentifierCase,
    pub current_user: Option<String>,
    pub database: Option<String>,
    /// `None` when the engine cannot report it (SQLite; engines without the view).
    pub tls_in_use: Option<bool>,
    pub latency_ms: Option<u64>,
}

/// The probe for an engine family, or `None` for kinds without a driver.
/// Every statement selects ONE row whose columns are, in order:
/// product, version, charset, collation, case_flag, user, database, tls.
pub fn probe_sql(db: DatabaseType) -> Option<&'static str> {
    match db.connection_kind() {
        ConnectionKind::Tds => Some(
            "SELECT 'Microsoft SQL Server' AS product, CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) AS version, NULL AS charset, CAST(SERVERPROPERTY('Collation') AS nvarchar(128)) AS collation, CASE WHEN CAST(SERVERPROPERTY('Collation') AS nvarchar(128)) LIKE '%_CS_%' THEN 1 ELSE 0 END AS case_flag, SUSER_SNAME() AS current_user_name, DB_NAME() AS database_name, (SELECT TOP 1 CASE encrypt_option WHEN 'TRUE' THEN 1 ELSE 0 END FROM sys.dm_exec_connections WHERE session_id = @@SPID) AS tls",
        ),
        ConnectionKind::PostgresWire => Some(
            "SELECT 'PostgreSQL' AS product, current_setting('server_version') AS version, current_setting('server_encoding') AS charset, current_setting('lc_collate') AS collation, 1 AS case_flag, current_user::text AS current_user_name, current_database()::text AS database_name, (SELECT CASE WHEN ssl THEN 1 ELSE 0 END FROM pg_stat_ssl WHERE pid = pg_backend_pid()) AS tls",
        ),
        ConnectionKind::MySqlWire => Some(
            "SELECT IF(VERSION() LIKE '%MariaDB%', 'MariaDB', 'MySQL') AS product, VERSION() AS version, @@character_set_server AS charset, @@collation_server AS collation, IF(@@lower_case_table_names = 0, 1, 0) AS case_flag, CURRENT_USER() AS current_user_name, DATABASE() AS database_name, (SELECT IF(VARIABLE_VALUE = '', 0, 1) FROM performance_schema.session_status WHERE VARIABLE_NAME = 'Ssl_cipher') AS tls",
        ),
        ConnectionKind::SqliteFile => Some(
            "SELECT 'SQLite' AS product, sqlite_version() AS version, 'UTF-8' AS charset, 'BINARY' AS collation, 0 AS case_flag, NULL AS current_user_name, 'main' AS database_name, NULL AS tls",
        ),
        // libSQL embeds SQLite; the server (sqld / Turso) reports no user
        // and TLS is decided by the URL the driver dialed.
        ConnectionKind::HranaHttp => Some(
            "SELECT 'libSQL' AS product, sqlite_version() AS version, 'UTF-8' AS charset, 'BINARY' AS collation, 0 AS case_flag, NULL AS current_user_name, 'main' AS database_name, NULL AS tls",
        ),
        ConnectionKind::DuckDbFile => Some(
            "SELECT 'DuckDB' AS product, version() AS version, 'UTF-8' AS charset, NULL AS collation, 0 AS case_flag, NULL AS current_user_name, current_database() AS database_name, NULL AS tls",
        ),
        // ClickHouse identifiers are case-sensitive; TLS is decided by the
        // URL scheme the driver dialed, which the server does not report.
        ConnectionKind::ClickHouseHttp => Some(
            "SELECT 'ClickHouse' AS product, version() AS version, 'UTF-8' AS charset, NULL AS collation, 1 AS case_flag, currentUser() AS current_user_name, currentDatabase() AS database_name, NULL AS tls",
        ),
        // Public dictionary views only (no v$ grants needed); the container
        // name is the PDB the session landed in.
        ConnectionKind::OracleNet => Some(
            "SELECT 'Oracle Database' AS product, (SELECT v.version FROM product_component_version v WHERE v.product LIKE 'Oracle%' FETCH FIRST 1 ROWS ONLY) AS version, (SELECT p.value FROM nls_database_parameters p WHERE p.parameter = 'NLS_CHARACTERSET') AS charset, (SELECT p.value FROM nls_session_parameters p WHERE p.parameter = 'NLS_SORT') AS collation, 1 AS case_flag, USER AS current_user_name, SYS_CONTEXT('USERENV', 'CON_NAME') AS database_name, CASE SYS_CONTEXT('USERENV', 'NETWORK_PROTOCOL') WHEN 'tcps' THEN 1 ELSE 0 END AS tls FROM DUAL",
        ),
        // Redis: the driver composes the row from INFO server + ACL WHOAMI.
        ConnectionKind::Resp => Some("SQLLY.SERVERINFO"),
        _ => None,
    }
}

fn probe_sql_minimal(db: DatabaseType) -> Option<&'static str> {
    match db.connection_kind() {
        ConnectionKind::Tds => Some(
            "SELECT 'Microsoft SQL Server' AS product, CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) AS version, NULL AS charset, CAST(SERVERPROPERTY('Collation') AS nvarchar(128)) AS collation, CASE WHEN CAST(SERVERPROPERTY('Collation') AS nvarchar(128)) LIKE '%_CS_%' THEN 1 ELSE 0 END AS case_flag, SUSER_SNAME() AS current_user_name, DB_NAME() AS database_name, NULL AS tls",
        ),
        ConnectionKind::PostgresWire => Some(
            "SELECT 'PostgreSQL' AS product, current_setting('server_version') AS version, current_setting('server_encoding') AS charset, current_setting('lc_collate') AS collation, 1 AS case_flag, current_user::text AS current_user_name, current_database()::text AS database_name, NULL AS tls",
        ),
        ConnectionKind::MySqlWire => Some(
            "SELECT IF(VERSION() LIKE '%MariaDB%', 'MariaDB', 'MySQL') AS product, VERSION() AS version, @@character_set_server AS charset, @@collation_server AS collation, IF(@@lower_case_table_names = 0, 1, 0) AS case_flag, CURRENT_USER() AS current_user_name, DATABASE() AS database_name, NULL AS tls",
        ),
        ConnectionKind::SqliteFile
        | ConnectionKind::HranaHttp
        | ConnectionKind::DuckDbFile
        | ConnectionKind::ClickHouseHttp
        | ConnectionKind::OracleNet
        | ConnectionKind::Resp => probe_sql(db),
        _ => None,
    }
}

fn cell_as_string(value: &CellValue) -> Option<String> {
    match value {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(b.to_string()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(f) => Some(f.to_string()),
        CellValue::String(s) if s.is_empty() => None,
        CellValue::String(s) => Some(s.clone()),
        CellValue::Bytes(_) => None,
    }
}

fn parse_flag(value: &CellValue) -> Option<bool> {
    match value {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(*b),
        CellValue::I64(1) => Some(true),
        CellValue::I64(0) => Some(false),
        CellValue::String(s) => match s.trim() {
            "1" | "true" | "TRUE" => Some(true),
            "0" | "false" | "FALSE" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn parse_identifier_case(value: &CellValue) -> IdentifierCase {
    match parse_flag(value) {
        Some(true) => IdentifierCase::Sensitive,
        Some(false) => IdentifierCase::Insensitive,
        None => IdentifierCase::Unknown,
    }
}

/// Parse the probe's single row (missing/NULL cells become `None`).
pub fn parse_probe_row(db: DatabaseType, row: &[CellValue]) -> ServerInfo {
    let mut info = ServerInfo {
        product: row.first().and_then(cell_as_string).unwrap_or_default(),
        version: row.get(1).and_then(cell_as_string).unwrap_or_default(),
        server_charset: row.get(2).and_then(cell_as_string),
        server_collation: row.get(3).and_then(cell_as_string),
        identifier_case: row
            .get(4)
            .map(parse_identifier_case)
            .unwrap_or(IdentifierCase::Unknown),
        current_user: row.get(5).and_then(cell_as_string),
        database: row.get(6).and_then(cell_as_string),
        tls_in_use: row.get(7).and_then(parse_flag),
        latency_ms: None,
    };

    if db.connection_kind() == ConnectionKind::PostgresWire
        && info.version.starts_with("CockroachDB")
    {
        info.product = "CockroachDB".to_string();
    }

    info
}

async fn execute_probe_row(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    sql: &str,
) -> StudioResult<Vec<CellValue>> {
    let sql = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    let handle = queries.execute(profile, &sql, 1).await?;
    let mut events = handle.events;
    let mut row: Option<Vec<CellValue>> = None;

    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Rows { index: 0, rows } if !rows.is_empty() => {
                row = Some(rows.into_iter().next().unwrap());
                break;
            }
            QueryEvent::Error { message } => {
                return Err(StudioError::QueryExecution(message));
            }
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }

    row.ok_or_else(|| {
        StudioError::QueryExecutionMessage(sqlly_core::DisplayMessage::new(
            "services.server_info.no_rows",
            "Query execution error: server info probe returned no rows",
        ))
    })
}

/// Run the probe through the port. `latency_ms` is filled by the caller
/// (it wraps the handshake, not this query).
pub async fn collect_server_info(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
) -> StudioResult<ServerInfo> {
    let db = profile.database_type;
    let Some(sql) = probe_sql(db) else {
        return Err(StudioError::CapabilityNotSupported(format!(
            "server info probe is not available for {}",
            db.display_name()
        )));
    };

    match execute_probe_row(queries, profile, sql).await {
        Ok(row) => Ok(parse_probe_row(db, &row)),
        Err(first) => {
            let Some(minimal) = probe_sql_minimal(db) else {
                return Err(first);
            };
            if minimal == sql {
                return Err(first);
            }
            execute_probe_row(queries, profile, minimal)
                .await
                .map(|row| parse_probe_row(db, &row))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sqlly_core::SecretProvider;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    struct FakeQueries {
        executed: Arc<Mutex<Vec<String>>>,
        responses: Mutex<Vec<Result<Vec<CellValue>, String>>>,
    }

    impl FakeQueries {
        fn new(responses: Vec<Result<Vec<CellValue>, String>>) -> Self {
            Self {
                executed: Arc::new(Mutex::new(Vec::new())),
                responses: Mutex::new(responses),
            }
        }

        fn executed(&self) -> Vec<String> {
            self.executed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl QueryExecutor for FakeQueries {
        async fn execute(
            &self,
            _profile: &ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> StudioResult<crate::query::QueryHandle> {
            self.executed.lock().unwrap().push(sql.to_string());
            let response = self.responses.lock().unwrap().remove(0);
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                match response {
                    Ok(row) => {
                        let _ = tx
                            .send(QueryEvent::ResultSet {
                                index: 0,
                                columns: vec![],
                            })
                            .await;
                        let _ = tx
                            .send(QueryEvent::Rows {
                                index: 0,
                                rows: vec![row],
                            })
                            .await;
                        let _ = tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await;
                    }
                    Err(message) => {
                        let _ = tx.send(QueryEvent::Error { message }).await;
                    }
                }
            });
            Ok(crate::query::QueryHandle {
                task_id: "server-info-test".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
            Ok(())
        }
    }

    fn profile(db_type: DatabaseType) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "server-info-test",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.database_type = db_type;
        profile
    }

    fn sample_row() -> Vec<CellValue> {
        vec![
            CellValue::String("MySQL".into()),
            CellValue::String("8.0.1".into()),
            CellValue::String("utf8mb4".into()),
            CellValue::String("utf8mb4_general_ci".into()),
            CellValue::I64(1),
            CellValue::String("root@localhost".into()),
            CellValue::String("app".into()),
            CellValue::I64(1),
        ]
    }

    #[test]
    fn probe_sql_exists_for_the_native_kinds_and_not_for_db2() {
        assert!(probe_sql(DatabaseType::SqlServer).is_some());
        assert!(probe_sql(DatabaseType::Postgres).is_some());
        assert!(probe_sql(DatabaseType::MySql).is_some());
        assert!(probe_sql(DatabaseType::Sqlite).is_some());
        let oracle = probe_sql(DatabaseType::Oracle).unwrap();
        assert!(oracle.contains("FROM DUAL"));
        assert!(oracle.contains("CON_NAME"));
        assert!(probe_sql(DatabaseType::Db2).is_none());
    }

    #[test]
    fn parse_probe_row_maps_case_and_tls_flags() {
        let row = sample_row();
        let info = parse_probe_row(DatabaseType::MySql, &row);
        assert_eq!(info.identifier_case, IdentifierCase::Sensitive);
        assert_eq!(info.tls_in_use, Some(true));

        let mut bool_row = row.clone();
        bool_row[4] = CellValue::Bool(false);
        bool_row[7] = CellValue::Bool(false);
        let info = parse_probe_row(DatabaseType::MySql, &bool_row);
        assert_eq!(info.identifier_case, IdentifierCase::Insensitive);
        assert_eq!(info.tls_in_use, Some(false));

        let mut str_row = row;
        str_row[4] = CellValue::String("0".into());
        str_row[7] = CellValue::String("0".into());
        let info = parse_probe_row(DatabaseType::MySql, &str_row);
        assert_eq!(info.identifier_case, IdentifierCase::Insensitive);
        assert_eq!(info.tls_in_use, Some(false));
    }

    #[tokio::test]
    async fn collect_server_info_returns_row_from_executor() {
        let queries = FakeQueries::new(vec![Ok(sample_row())]);
        let info = collect_server_info(&queries, &profile(DatabaseType::MySql))
            .await
            .unwrap();
        assert_eq!(info.product, "MySQL");
        assert_eq!(info.version, "8.0.1");
        assert_eq!(info.database.as_deref(), Some("app"));
        assert!(!queries.executed().is_empty());
    }

    #[tokio::test]
    async fn collect_server_info_retries_without_tls_on_error() {
        let mut minimal_row = sample_row();
        minimal_row[7] = CellValue::Null;
        let queries = FakeQueries::new(vec![
            Err("permission denied on sys.dm_exec_connections".into()),
            Ok(minimal_row),
        ]);
        let info = collect_server_info(&queries, &profile(DatabaseType::SqlServer))
            .await
            .unwrap();
        assert_eq!(info.tls_in_use, None);
        let executed = queries.executed();
        assert_eq!(executed.len(), 2);
        assert!(executed[0].contains("sys.dm_exec_connections"));
        assert!(executed[1].contains("NULL AS tls"));
    }

    #[tokio::test]
    async fn collect_server_info_rejects_unsupported_engine() {
        let queries = FakeQueries::new(vec![]);
        let err = collect_server_info(&queries, &profile(DatabaseType::Db2))
            .await
            .unwrap_err();
        assert!(matches!(err, StudioError::CapabilityNotSupported(_)));
    }
}
