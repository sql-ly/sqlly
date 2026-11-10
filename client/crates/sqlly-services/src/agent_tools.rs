//! Live metadata + query executor backing for agent read-only tools.

use std::sync::Arc;

use async_trait::async_trait;
use sqlly_ai::catalog::{AgentToolHost, MAX_CELL_CHARS};
use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_intel::query::safety::classify_select_only_for;
use sqlly_lang::sqlmodel::DatabaseType;

use crate::metadata::{MetadataNode, MetadataService};
use crate::query::{QueryEvent, QueryExecutor};

const MAX_LIST_TABLE_LINES: usize = 400;

/// Executes the four built-in agent tools against real services.
pub struct ServicesToolHost {
    profile: ConnectionProfile,
    metadata: Arc<dyn MetadataService>,
    queries: Arc<dyn QueryExecutor>,
}

impl ServicesToolHost {
    pub fn new(
        profile: ConnectionProfile,
        metadata: Arc<dyn MetadataService>,
        queries: Arc<dyn QueryExecutor>,
    ) -> Self {
        Self {
            profile,
            metadata,
            queries,
        }
    }

    fn database(&self) -> Result<&str, String> {
        self.profile
            .database
            .as_deref()
            .filter(|db| !db.is_empty())
            .ok_or_else(|| "connect to a database first".to_string())
    }

    fn split_table(table: &str) -> (Option<String>, String) {
        let trimmed = table.trim();
        if let Some((schema, name)) = trimmed.rsplit_once('.') {
            let schema = schema.trim();
            let name = name.trim();
            if !schema.is_empty() && !name.is_empty() {
                return (Some(schema.to_string()), name.to_string());
            }
        }
        (None, trimmed.to_string())
    }

    fn default_schema(&self) -> String {
        match self.profile.database_type {
            DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => "dbo".into(),
            DatabaseType::Postgres => "public".into(),
            DatabaseType::MySql | DatabaseType::MariaDb => self
                .profile
                .database
                .clone()
                .unwrap_or_else(|| "mysql".into()),
            DatabaseType::Sqlite => "main".into(),
            other => other.default_schema().unwrap_or("dbo").to_string(),
        }
    }

    fn quote_table(&self, schema: &str, name: &str) -> String {
        let db = self.profile.database_type;
        format!(
            "{}.{}",
            db.quote_identifier(schema),
            db.quote_identifier(name)
        )
    }

    fn sample_sql(&self, schema: &str, name: &str, limit: usize) -> String {
        let quoted = self.quote_table(schema, name);
        match self.profile.database_type {
            DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => {
                format!("SELECT TOP ({limit}) * FROM {quoted}")
            }
            _ => format!("SELECT * FROM {quoted} LIMIT {limit}"),
        }
    }

    fn window_cell(cell: &CellValue) -> String {
        let text = match cell {
            CellValue::Null => "NULL".into(),
            CellValue::Bool(b) => b.to_string(),
            CellValue::I64(n) => n.to_string(),
            CellValue::F64(n) => n.to_string(),
            CellValue::String(s) => s.clone(),
            CellValue::Bytes(b) => format!("<bytes {}>", b.len()),
        };
        if text.chars().count() > MAX_CELL_CHARS {
            format!("{}…", text.chars().take(MAX_CELL_CHARS).collect::<String>())
        } else {
            text
        }
    }

    /// The profile every agent-composed query runs under: the SELECT-only
    /// parser gate plus the read-only mode the database itself enforces
    /// (read-only transaction always rolled back; `query_only` on SQLite;
    /// rollback wrap plus statement screen on SQL Server). The user's own
    /// profile is left untouched — their editor runs never carry this.
    fn read_only_profile(&self) -> ConnectionProfile {
        let mut profile = self.profile.clone();
        profile.block_all_but_select = true;
        profile.enforce_read_only = true;
        profile
    }

    /// Turn an engine refusal under the read-only mode into the plain message
    /// the tool result should carry; any other error passes through.
    fn explain_error(profile: &ConnectionProfile, message: String) -> String {
        sqlly_intel::query::safety::explain_read_only_refusal(profile.database_type, &message)
            .unwrap_or(message)
    }

    async fn collect_limited_rows(
        &self,
        sql: &str,
        limit: usize,
    ) -> Result<(Vec<String>, Vec<Vec<String>>, bool), String> {
        let profile = self.read_only_profile();
        // Gate here for a clear early refusal, but hand the driver the RAW
        // SQL: the driver applies the policy (and the read-only wrap) itself,
        // exactly once.
        sqlly_intel::query::safety::check_connection_policy_for(&profile, sql)
            .map_err(|error| error.to_string())?;
        let handle = self
            .queries
            .execute(&profile, sql, 256)
            .await
            .map_err(|error| Self::explain_error(&profile, error.to_string()))?;
        let task_id = handle.task_id.clone();
        let mut events = handle.events;
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut cancelled = false;
        let mut saw_second_result_set = false;

        while let Some(event) = events.recv().await {
            match event {
                QueryEvent::ResultSet {
                    index,
                    columns: cols,
                } => {
                    if index > 0 {
                        saw_second_result_set = true;
                        break;
                    }
                    columns = cols.iter().map(|c| c.name.clone()).collect();
                }
                QueryEvent::Rows { index, rows: batch } => {
                    if index > 0 {
                        saw_second_result_set = true;
                        break;
                    }
                    for row in batch {
                        if rows.len() >= limit {
                            cancelled = true;
                            let _ = self.queries.cancel(&task_id).await;
                            break;
                        }
                        rows.push(row.iter().map(Self::window_cell).collect());
                    }
                    if cancelled {
                        while events.recv().await.is_some() {}
                        break;
                    }
                }
                QueryEvent::Error { message } => {
                    return Err(Self::explain_error(&profile, message));
                }
                QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                _ => {}
            }
        }

        Ok((columns, rows, cancelled || saw_second_result_set))
    }

    fn render_pipe_table(columns: &[String], rows: &[Vec<String>], more_available: bool) -> String {
        if columns.is_empty() {
            return "(no columns)".into();
        }
        let mut lines = vec![columns.join(" | ")];
        for row in rows {
            let mut cells = row.clone();
            while cells.len() < columns.len() {
                cells.push(String::new());
            }
            lines.push(cells.join(" | "));
        }
        let trailer = if more_available {
            format!("({} rows shown, more available)", rows.len())
        } else {
            format!("({} rows shown)", rows.len())
        };
        lines.push(trailer);
        lines.join("\n")
    }
}

#[async_trait]
impl AgentToolHost for ServicesToolHost {
    async fn list_tables(&self, schema: Option<&str>) -> Result<String, String> {
        let database = self.database()?;
        let nodes = self
            .metadata
            .list_tables(&self.profile, database, schema)
            .await
            .map_err(|error| error.to_string())?;
        let mut lines: Vec<String> = nodes
            .iter()
            .map(|MetadataNode { schema, name, .. }| match schema {
                Some(s) => format!("{s}.{name}"),
                None => name.clone(),
            })
            .collect();
        if lines.len() > MAX_LIST_TABLE_LINES {
            let extra = lines.len() - MAX_LIST_TABLE_LINES;
            lines.truncate(MAX_LIST_TABLE_LINES);
            lines.push(format!("… {extra} more; pass a schema to narrow"));
        }
        Ok(lines.join("\n"))
    }

    async fn get_columns(&self, table: &str) -> Result<String, String> {
        let database = self.database()?;
        let (schema, name) = Self::split_table(table);
        let schema = schema.unwrap_or_else(|| self.default_schema());
        let nodes = self
            .metadata
            .list_columns(&self.profile, database, &schema, &name)
            .await
            .map_err(|error| error.to_string())?;
        Ok(nodes
            .iter()
            .map(|MetadataNode { name, detail, .. }| {
                format!("{name}  {}", detail.as_deref().unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn get_sample_rows(&self, table: &str, limit: usize) -> Result<String, String> {
        let (schema, name) = Self::split_table(table);
        let schema = schema.unwrap_or_else(|| self.default_schema());
        let sql = self.sample_sql(&schema, &name, limit);
        self.run_readonly_query(&sql, limit).await
    }

    async fn run_readonly_query(&self, sql: &str, limit: usize) -> Result<String, String> {
        // Read the SQL the way this connection's engine reads it (every
        // statement it would run, under every lexical reading).
        let classification = classify_select_only_for(self.profile.database_type, sql);
        if !classification.select_only {
            let reason = classification
                .reason
                .unwrap_or_else(|| "not a SELECT".into());
            return Err(format!(
                "refused: {reason}; only SELECT statements may run in agent mode"
            ));
        }
        let (columns, rows, more) = self.collect_limited_rows(sql, limit).await?;
        let mut rendered = Self::render_pipe_table(&columns, &rows, more);
        if more {
            rendered.push_str("\n(note: only the first result set is shown)");
        }
        Ok(rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sqlly_core::SecretProvider;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    struct FakeMetadata {
        tables: Vec<MetadataNode>,
        columns: Vec<MetadataNode>,
    }

    #[async_trait]
    impl MetadataService for FakeMetadata {
        async fn list_schemas(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
        ) -> sqlly_core::StudioResult<Vec<String>> {
            Ok(vec![])
        }

        async fn list_tables(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: Option<&str>,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(self.tables.clone())
        }

        async fn list_views(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: Option<&str>,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }

        async fn list_programmable_objects(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: Option<&str>,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }

        async fn list_columns(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: &str,
            _object: &str,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(self.columns.clone())
        }

        async fn list_procedure_parameters(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: &str,
            _object: &str,
        ) -> sqlly_core::StudioResult<Vec<crate::intellisense::procedures::ProcedureParameter>>
        {
            Ok(vec![])
        }

        async fn list_indexes(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: &str,
            _table: &str,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }

        async fn list_triggers(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: &str,
            _table: &str,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }

        async fn list_constraints(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: &str,
            _table: &str,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }

        async fn list_statistics(
            &self,
            _profile: &ConnectionProfile,
            _database: &str,
            _schema: &str,
            _table: &str,
        ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }
    }

    struct FakeQueries {
        executed: Mutex<Vec<String>>,
        cancelled: Mutex<usize>,
        row_count: usize,
    }

    #[async_trait]
    impl QueryExecutor for FakeQueries {
        async fn execute(
            &self,
            _profile: &ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
            self.executed.lock().unwrap().push(sql.to_string());
            let (tx, rx) = mpsc::channel(16);
            let row_count = self.row_count;
            tokio::spawn(async move {
                let _ = tx
                    .send(QueryEvent::ResultSet {
                        index: 0,
                        columns: vec![sqlly_core::ResultColumn {
                            name: "id".into(),
                            data_type: "int".into(),
                            nullable: true,
                        }],
                    })
                    .await;
                let mut sent = 0usize;
                while sent < row_count {
                    let batch: Vec<Vec<CellValue>> = (0..16.min(row_count - sent))
                        .map(|n| vec![CellValue::I64((sent + n) as i64)])
                        .collect();
                    sent += batch.len();
                    let _ = tx
                        .send(QueryEvent::Rows {
                            index: 0,
                            rows: batch,
                        })
                        .await;
                }
                let _ = tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await;
            });
            Ok(crate::query::QueryHandle {
                task_id: "agent-test".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
            *self.cancelled.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn profile(db_type: DatabaseType) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "agent-test",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.database = Some("app".into());
        profile.database_type = db_type;
        profile
    }

    #[tokio::test]
    async fn readonly_query_refuses_non_select_without_executing() {
        let host = ServicesToolHost::new(
            profile(DatabaseType::SqlServer),
            Arc::new(FakeMetadata {
                tables: vec![],
                columns: vec![],
            }),
            Arc::new(FakeQueries {
                executed: Mutex::new(vec![]),
                cancelled: Mutex::new(0),
                row_count: 0,
            }),
        );
        let err = host
            .run_readonly_query("DELETE FROM t", 10)
            .await
            .unwrap_err();
        assert!(err.contains("refused"));
    }

    /// Records the profile each run was handed; emits one row, or a terminal
    /// error when `error` is set.
    struct ProfileCapture {
        profiles: Mutex<Vec<ConnectionProfile>>,
        executed: Mutex<Vec<String>>,
        error: Option<String>,
    }

    #[async_trait]
    impl QueryExecutor for ProfileCapture {
        async fn execute(
            &self,
            profile: &ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
            self.profiles.lock().unwrap().push(profile.clone());
            self.executed.lock().unwrap().push(sql.to_string());
            let (tx, rx) = mpsc::channel(4);
            let error = self.error.clone();
            tokio::spawn(async move {
                match error {
                    Some(message) => {
                        let _ = tx.send(QueryEvent::Error { message }).await;
                    }
                    None => {
                        let _ = tx
                            .send(QueryEvent::ResultSet {
                                index: 0,
                                columns: vec![sqlly_core::ResultColumn {
                                    name: "n".into(),
                                    data_type: "int".into(),
                                    nullable: true,
                                }],
                            })
                            .await;
                        let _ = tx
                            .send(QueryEvent::Rows {
                                index: 0,
                                rows: vec![vec![CellValue::I64(1)]],
                            })
                            .await;
                        let _ = tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await;
                    }
                }
            });
            Ok(crate::query::QueryHandle {
                task_id: "capture".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
            Ok(())
        }
    }

    fn capture_host(
        db_type: DatabaseType,
        error: Option<&str>,
    ) -> (ServicesToolHost, Arc<ProfileCapture>) {
        let queries = Arc::new(ProfileCapture {
            profiles: Mutex::new(vec![]),
            executed: Mutex::new(vec![]),
            error: error.map(str::to_string),
        });
        let host = ServicesToolHost::new(
            profile(db_type),
            Arc::new(FakeMetadata {
                tables: vec![],
                columns: vec![],
            }),
            Arc::clone(&queries) as Arc<dyn QueryExecutor>,
        );
        (host, queries)
    }

    #[tokio::test]
    async fn readonly_query_runs_raw_sql_under_a_database_enforced_profile() {
        let (host, queries) = capture_host(DatabaseType::Postgres, None);
        host.run_readonly_query("SELECT 1", 10).await.unwrap();
        // The driver wraps exactly once, so the host must hand it raw SQL...
        assert_eq!(queries.executed.lock().unwrap()[0], "SELECT 1");
        // ...under a profile that carries both layers, without touching the
        // user's own profile.
        let ran_under = &queries.profiles.lock().unwrap()[0];
        assert!(ran_under.enforce_read_only);
        assert!(ran_under.block_all_but_select);
        assert!(!host.profile.enforce_read_only);
    }

    #[tokio::test]
    async fn readonly_query_screens_sql_server_side_effects_before_executing() {
        let (host, queries) = capture_host(DatabaseType::SqlServer, None);
        let err = host
            .run_readonly_query("SELECT * FROM OPENQUERY(linked, 'SELECT 1')", 10)
            .await
            .unwrap_err();
        assert!(err.contains("blocked: AI queries run read-only"), "{err}");
        assert!(queries.executed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn database_read_only_refusal_is_explained_not_raw() {
        let (host, _) = capture_host(
            DatabaseType::Postgres,
            Some("ERROR: cannot execute INSERT in a read-only transaction"),
        );
        let err = host
            .run_readonly_query("SELECT sneaky_write()", 10)
            .await
            .unwrap_err();
        assert!(
            err.starts_with("blocked: AI queries run read-only"),
            "{err}"
        );
        assert!(err.contains("cannot execute INSERT"), "{err}");

        // Unrelated engine errors pass through untouched.
        let (host, _) = capture_host(
            DatabaseType::Postgres,
            Some("relation \"t\" does not exist"),
        );
        let err = host
            .run_readonly_query("SELECT * FROM t", 10)
            .await
            .unwrap_err();
        assert_eq!(err, "relation \"t\" does not exist");
    }

    #[tokio::test]
    async fn readonly_query_caps_rows_and_cancels() {
        let queries = Arc::new(FakeQueries {
            executed: Mutex::new(vec![]),
            cancelled: Mutex::new(0),
            row_count: 500,
        });
        let cancelled = Arc::clone(&queries);
        let host = ServicesToolHost::new(
            profile(DatabaseType::SqlServer),
            Arc::new(FakeMetadata {
                tables: vec![],
                columns: vec![],
            }),
            queries,
        );
        let rendered = host
            .run_readonly_query("SELECT * FROM t", 100)
            .await
            .unwrap();
        assert!(rendered.contains("100 rows shown"));
        assert!(rendered.contains("more available"));
        assert_eq!(*cancelled.cancelled.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn sample_rows_uses_dialect_limit_syntax() {
        let sqlserver_queries = Arc::new(FakeQueries {
            executed: Mutex::new(vec![]),
            cancelled: Mutex::new(0),
            row_count: 1,
        });
        let host = ServicesToolHost::new(
            profile(DatabaseType::SqlServer),
            Arc::new(FakeMetadata {
                tables: vec![],
                columns: vec![],
            }),
            Arc::clone(&sqlserver_queries) as Arc<dyn QueryExecutor>,
        );
        host.get_sample_rows("dbo.users", 20).await.unwrap();
        let sql = sqlserver_queries.executed.lock().unwrap()[0].clone();
        assert!(sql.starts_with("SELECT TOP (20)"));

        let postgres_queries = Arc::new(FakeQueries {
            executed: Mutex::new(vec![]),
            cancelled: Mutex::new(0),
            row_count: 1,
        });
        let host = ServicesToolHost::new(
            profile(DatabaseType::Postgres),
            Arc::new(FakeMetadata {
                tables: vec![],
                columns: vec![],
            }),
            Arc::clone(&postgres_queries) as Arc<dyn QueryExecutor>,
        );
        host.get_sample_rows("users", 20).await.unwrap();
        let sql = postgres_queries.executed.lock().unwrap()[0].clone();
        assert!(sql.ends_with("LIMIT 20"));
    }

    #[tokio::test]
    async fn list_tables_requires_database() {
        let mut profile = profile(DatabaseType::SqlServer);
        profile.database = None;
        let host = ServicesToolHost::new(
            profile,
            Arc::new(FakeMetadata {
                tables: vec![],
                columns: vec![],
            }),
            Arc::new(FakeQueries {
                executed: Mutex::new(vec![]),
                cancelled: Mutex::new(0),
                row_count: 0,
            }),
        );
        let err = host.list_tables(None).await.unwrap_err();
        assert!(err.contains("connect to a database"));
    }
}
