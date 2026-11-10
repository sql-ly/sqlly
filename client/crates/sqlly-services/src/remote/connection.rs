//! Remote [`ConnectionManager`]: composed over the remote query path.
//!
//! The engine exposes no dedicated `test_connection` / `list_databases`
//! requests, so both operations run their SQL through the remote
//! [`QueryExecutor`] — exactly the metadata SQL the in-process
//! `DefaultMetadataService` would run, but tunneled through the engine.
//! `test_connection` runs `SELECT 1`; `list_databases` runs the profile's
//! engine's database-listing metadata query (`sys.databases` on SQL Server,
//! `pg_database` on PostgreSQL, …) and parses the rows.

use std::sync::Arc;

use crate::{QueryEvent, QueryExecutor};
use async_trait::async_trait;
use sqlly_core::{CellValue, ConnectionProfile, DatabaseSummary, StudioError, StudioResult};
use sqlly_intel::metadata::{MetadataQuery, sql_for, sql_for_engine};

/// Remote connection manager that drives its probes through a shared remote
/// [`QueryExecutor`].
pub struct RemoteConnectionManager {
    queries: Arc<dyn QueryExecutor>,
}

impl RemoteConnectionManager {
    pub fn new(queries: Arc<dyn QueryExecutor>) -> Self {
        Self { queries }
    }

    /// Execute `sql` and collect every row, surfacing the first `Error` event as
    /// a `StudioError`.
    async fn collect_rows(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
    ) -> StudioResult<Vec<Vec<CellValue>>> {
        // Full values: these rows are read as data, never painted as previews.
        let handle = self
            .queries
            .execute_full_values(profile, None, sql, 1000)
            .await?;
        let mut events = handle.events;
        let mut rows: Vec<Vec<CellValue>> = Vec::new();
        while let Some(event) = events.recv().await {
            match event {
                QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
                QueryEvent::Error { message } => return Err(StudioError::Connection(message)),
                QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                _ => {}
            }
        }
        Ok(rows)
    }
}

#[async_trait]
impl crate::ConnectionManager for RemoteConnectionManager {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        // A trivial round trip proves the engine reached the server and ran a
        // statement; any connection failure surfaces as an `Error` event.
        self.collect_rows(profile, "SELECT 1").await.map(|_| ())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        // The engine's own catalog: a SQL Server query on PostgreSQL fails
        // the whole connect ("relation sys.databases does not exist").
        let sql = sql_for_engine(&MetadataQuery::Databases, profile.database_type)
            .unwrap_or_else(|_| sql_for(&MetadataQuery::Databases));
        let rows = self.collect_rows(profile, &sql).await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let mut cells = row.into_iter();
                let name = match cells.next() {
                    Some(CellValue::String(s)) => s,
                    Some(CellValue::Null) | None => return None,
                    Some(other) => cell_to_string(&other),
                };
                let state = match cells.next() {
                    Some(CellValue::String(s)) => s,
                    Some(other) => cell_to_string(&other),
                    None => String::new(),
                };
                let compatibility_level = match cells.next() {
                    Some(CellValue::I64(n)) => u16::try_from(n).ok(),
                    Some(CellValue::F64(n)) => u16::try_from(n as i64).ok(),
                    _ => None,
                };
                Some(DatabaseSummary {
                    name,
                    state,
                    compatibility_level,
                })
            })
            .collect())
    }
}

fn cell_to_string(cell: &CellValue) -> String {
    match cell {
        CellValue::Null => String::new(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(n) => n.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConnectionManager as _, QueryHandle};
    use sqlly_core::DatabaseType;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// Records the SQL it is asked to run and answers one database row.
    #[derive(Default)]
    struct RecordingQueries {
        sql: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl QueryExecutor for RecordingQueries {
        async fn execute(
            &self,
            _profile: &ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> StudioResult<QueryHandle> {
            self.sql.lock().unwrap().push(sql.to_string());
            let (tx, rx) = mpsc::channel(4);
            tx.send(QueryEvent::Rows {
                index: 0,
                rows: vec![vec![
                    CellValue::String("shop".into()),
                    CellValue::String("ONLINE".into()),
                    CellValue::Null,
                ]],
            })
            .await
            .unwrap();
            tx.send(QueryEvent::Completed { elapsed_ms: 1 })
                .await
                .unwrap();
            Ok(QueryHandle {
                task_id: "t".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn list_databases_speaks_the_profiles_engine() {
        for (engine, expected) in [
            (DatabaseType::Postgres, "pg_database"),
            (DatabaseType::SqlServer, "sys.databases"),
        ] {
            let queries = Arc::new(RecordingQueries::default());
            let manager = RemoteConnectionManager::new(queries.clone());
            let mut profile = ConnectionProfile::sql_login(
                "Shop",
                "db.internal",
                "u",
                sqlly_core::SecretRef {
                    provider: sqlly_core::SecretProvider::InMemory,
                    key: String::new(),
                },
            );
            profile.database_type = engine;
            let databases = manager.list_databases(&profile).await.unwrap();
            assert_eq!(databases[0].name, "shop");
            let sql = queries.sql.lock().unwrap().join("\n");
            assert!(sql.contains(expected), "{engine:?} ran: {sql}");
        }
    }
}
