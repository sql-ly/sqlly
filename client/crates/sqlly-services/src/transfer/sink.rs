//! Native driver sink for parameterized transfer writes.

use async_trait::async_trait;
use sqlly_core::{ConnectionProfile, DatabaseType, SqlParam, StudioError, StudioResult};

/// Parameterized write surface for data transfer. Plans 154 and 159 reuse this trait.
#[async_trait]
pub trait TransferSink: Send {
    fn database_type(&self) -> DatabaseType;
    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> StudioResult<u64>;
    async fn begin(&mut self) -> StudioResult<()>;
    async fn commit(&mut self) -> StudioResult<()>;
    async fn rollback(&mut self) -> StudioResult<()>;
}

pub enum NativeSink {
    Tds(Box<sqlly_engines::tds::TdsClient>, DatabaseType),
    Pg(sqlly_engines::pg::PgClient, DatabaseType),
    MySql(sqlly_engines::mysql::MySqlClient, DatabaseType),
    Sqlite(sqlly_engines::sqlite::SqliteClient, DatabaseType),
    /// libSQL / Turso over Hrana: the SQLite sink's DDL and batched
    /// multi-row `INSERT … VALUES (?, …)` statements, each sent as one
    /// request with typed arguments on a server-side stream that
    /// [`LibSqlSink::begin`] opens with `BEGIN` and `commit`/`rollback`
    /// close, so a table's whole transfer is one server transaction.
    LibSql(LibSqlSink, DatabaseType),
    /// In-process DuckDB: batched multi-row `INSERT … VALUES (?, …)` inside
    /// one transaction per table, the same shape as the SQLite sink.
    DuckDb(sqlly_engines::duckdb::DuckDbClient, DatabaseType),
    /// ClickHouse over HTTP: parameters are bound as literals into each
    /// multi-row `INSERT … VALUES`; there are no transactions, so
    /// begin/commit/rollback are no-ops and each statement stands alone.
    ClickHouse(
        Box<sqlly_engines::clickhouse::ClickHouseClient>,
        DatabaseType,
    ),
    /// A session opened with autocommit off, so `COMMIT` / `ROLLBACK` below
    /// mean something; Oracle starts a transaction implicitly on the first
    /// statement, which is why [`TransferSink::begin`] has nothing to send.
    Oracle(sqlly_engines::oracle::OracleClient, DatabaseType),
}

/// Session formats so ISO-8601 text from any source engine (`2024-01-02`,
/// `2024-01-02 03:04:05`, `…05.123`) converts implicitly into `DATE` and
/// `TIMESTAMP` columns; Oracle allows trailing format elements to be absent
/// from the input.
const ORACLE_SESSION_FORMATS: &str = "ALTER SESSION SET \
NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' \
NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' \
NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'";

/// The libSQL client plus the stream carrying the current transaction.
pub struct LibSqlSink {
    client: sqlly_engines::libsql::LibSqlClient,
    stream: Option<sqlly_engines::libsql::LibSqlStream>,
}

impl LibSqlSink {
    pub fn new(client: sqlly_engines::libsql::LibSqlClient) -> Self {
        Self {
            client,
            stream: None,
        }
    }

    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> StudioResult<u64> {
        match self.stream.as_mut() {
            Some(stream) => Ok(stream.execute(sql, params).await?.affected_rows),
            None => {
                sqlly_engines::libsql::execute_params_on_client(&self.client, sql, params).await
            }
        }
    }

    async fn begin(&mut self) -> StudioResult<()> {
        if self.stream.is_some() {
            return Err(StudioError::QueryExecution(
                "a libSQL transfer transaction is already open".into(),
            ));
        }
        let mut stream = sqlly_engines::libsql::LibSqlStream::open(self.client.clone());
        stream.execute("BEGIN IMMEDIATE", &[]).await?;
        self.stream = Some(stream);
        Ok(())
    }

    async fn finish(&mut self, verb: &str) -> StudioResult<()> {
        let Some(mut stream) = self.stream.take() else {
            // Nothing open (the transaction failed to begin): nothing to end.
            return Ok(());
        };
        stream.execute_closing(verb, &[]).await.map(|_| ())
    }
}

pub async fn open_sink(profile: &ConnectionProfile) -> StudioResult<NativeSink> {
    use sqlly_core::ConnectionKind;
    let db = profile.database_type;
    match profile.database_type.connection_kind() {
        ConnectionKind::Tds => Ok(NativeSink::Tds(
            Box::new(sqlly_engines::tds::open_client(profile).await?),
            db,
        )),
        ConnectionKind::PostgresWire => Ok(NativeSink::Pg(
            sqlly_engines::pg::open_client(profile).await?,
            db,
        )),
        ConnectionKind::MySqlWire => Ok(NativeSink::MySql(
            sqlly_engines::mysql::open_client_found_rows(profile).await?,
            db,
        )),
        ConnectionKind::SqliteFile => Ok(NativeSink::Sqlite(
            sqlly_engines::sqlite::open_client(profile).await?,
            db,
        )),
        ConnectionKind::HranaHttp => Ok(NativeSink::LibSql(
            LibSqlSink::new(sqlly_engines::libsql::open_client(profile).await?),
            db,
        )),
        ConnectionKind::DuckDbFile => Ok(NativeSink::DuckDb(
            sqlly_engines::duckdb::open_client(profile).await?,
            db,
        )),
        ConnectionKind::ClickHouseHttp => Ok(NativeSink::ClickHouse(
            Box::new(sqlly_engines::clickhouse::open_client(profile).await?),
            db,
        )),
        ConnectionKind::OracleNet => {
            let client = sqlly_engines::oracle::open_client_manual_commit(profile).await?;
            sqlly_engines::oracle::execute_batch_on_client(&client, ORACLE_SESSION_FORMATS).await?;
            Ok(NativeSink::Oracle(client, db))
        }
        kind => Err(StudioError::CapabilityNotSupported(format!(
            "data transfer is not supported for {} ({kind:?} connections)",
            profile.database_type.display_name()
        ))),
    }
}

#[async_trait]
impl TransferSink for NativeSink {
    fn database_type(&self) -> DatabaseType {
        match self {
            NativeSink::Tds(_, db)
            | NativeSink::Pg(_, db)
            | NativeSink::MySql(_, db)
            | NativeSink::Sqlite(_, db)
            | NativeSink::LibSql(_, db)
            | NativeSink::DuckDb(_, db)
            | NativeSink::ClickHouse(_, db)
            | NativeSink::Oracle(_, db) => *db,
        }
    }

    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> StudioResult<u64> {
        match self {
            NativeSink::Tds(client, _) => {
                sqlly_engines::tds::execute_params_on_client(client, sql, params).await
            }
            NativeSink::Pg(client, _) => {
                sqlly_engines::pg::execute_params_on_client(client, sql, params).await
            }
            NativeSink::MySql(client, _) => {
                sqlly_engines::mysql::execute_params_on_client(client, sql, params).await
            }
            NativeSink::Sqlite(client, _) => {
                sqlly_engines::sqlite::execute_params_on_client(client, sql, params).await
            }
            NativeSink::LibSql(sink, _) => sink.execute(sql, params).await,
            NativeSink::DuckDb(client, _) => {
                sqlly_engines::duckdb::execute_params_on_client(client, sql, params).await
            }
            NativeSink::ClickHouse(client, _) => {
                sqlly_engines::clickhouse::execute_params_on_client(client, sql, params).await
            }
            NativeSink::Oracle(client, _) => {
                // A boolean bind is a PL/SQL-only type before Oracle 23ai;
                // booleans land in NUMBER(1) columns as 1 / 0.
                let params: Vec<SqlParam> = params
                    .iter()
                    .map(|p| match p {
                        SqlParam::Bool(b) => SqlParam::I64(i64::from(*b)),
                        other => other.clone(),
                    })
                    .collect();
                sqlly_engines::oracle::execute_params_on_client(client, sql, &params).await
            }
        }
    }

    async fn begin(&mut self) -> StudioResult<()> {
        match self {
            NativeSink::Tds(client, _) => {
                sqlly_engines::tds::execute_batch_on_client(client, "BEGIN TRANSACTION").await
            }
            NativeSink::Pg(client, _) => {
                sqlly_engines::pg::execute_batch_on_client(client, "BEGIN").await
            }
            NativeSink::MySql(client, _) => {
                sqlly_engines::mysql::execute_batch_on_client(client, "START TRANSACTION").await
            }
            NativeSink::Sqlite(client, _) => {
                sqlly_engines::sqlite::execute_params_on_client(client, "BEGIN IMMEDIATE", &[])
                    .await?;
                Ok(())
            }
            NativeSink::LibSql(sink, _) => sink.begin().await,
            NativeSink::DuckDb(client, _) => {
                sqlly_engines::duckdb::execute_batch_on_client(client, "BEGIN TRANSACTION").await
            }
            NativeSink::ClickHouse(_, _) => Ok(()),
            // Oracle: the session is already in manual-commit mode and the
            // first DML statement opens the transaction.
            NativeSink::Oracle(_, _) => Ok(()),
        }
    }

    async fn commit(&mut self) -> StudioResult<()> {
        match self {
            NativeSink::Tds(client, _) => {
                sqlly_engines::tds::execute_batch_on_client(client, "COMMIT").await
            }
            NativeSink::Pg(client, _) => {
                sqlly_engines::pg::execute_batch_on_client(client, "COMMIT").await
            }
            NativeSink::MySql(client, _) => {
                sqlly_engines::mysql::execute_batch_on_client(client, "COMMIT").await
            }
            NativeSink::Sqlite(client, _) => {
                sqlly_engines::sqlite::execute_params_on_client(client, "COMMIT", &[]).await?;
                Ok(())
            }
            NativeSink::LibSql(sink, _) => sink.finish("COMMIT").await,
            NativeSink::DuckDb(client, _) => {
                sqlly_engines::duckdb::execute_batch_on_client(client, "COMMIT").await
            }
            NativeSink::ClickHouse(_, _) => Ok(()),
            NativeSink::Oracle(client, _) => {
                sqlly_engines::oracle::execute_params_on_client(client, "COMMIT", &[]).await?;
                Ok(())
            }
        }
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        match self {
            NativeSink::Tds(client, _) => {
                sqlly_engines::tds::execute_batch_on_client(client, "ROLLBACK").await
            }
            NativeSink::Pg(client, _) => {
                sqlly_engines::pg::execute_batch_on_client(client, "ROLLBACK").await
            }
            NativeSink::MySql(client, _) => {
                sqlly_engines::mysql::execute_batch_on_client(client, "ROLLBACK").await
            }
            NativeSink::Sqlite(client, _) => {
                sqlly_engines::sqlite::execute_params_on_client(client, "ROLLBACK", &[]).await?;
                Ok(())
            }
            NativeSink::LibSql(sink, _) => sink.finish("ROLLBACK").await,
            NativeSink::DuckDb(client, _) => {
                sqlly_engines::duckdb::execute_batch_on_client(client, "ROLLBACK").await
            }
            // Nothing to undo: rows already written by earlier statements
            // stay; the runner reports the failing table.
            NativeSink::ClickHouse(_, _) => Ok(()),
            NativeSink::Oracle(client, _) => {
                sqlly_engines::oracle::execute_params_on_client(client, "ROLLBACK", &[]).await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::plan::{
        NameCasing, TransferColumn, TransferMode, TransferPlan, TransferTable,
    };
    use crate::transfer::run::{TransferCancel, run_transfer};
    use sqlly_core::{CellValue, ConnectionProfile, SecretProvider, SecretRef};

    fn file_profile(db: DatabaseType, path: &std::path::Path) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "transfer",
            path.to_string_lossy().as_ref(),
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = db;
        profile
    }

    /// SQLite source → DuckDB sink through the real runner: the DuckDB sink
    /// creates the table, batches parameterized inserts inside one
    /// transaction per table, and the rows come back intact.
    #[tokio::test]
    async fn sqlite_rows_transfer_into_a_duckdb_sink() {
        let dir = tempfile::tempdir().unwrap();
        let source = file_profile(DatabaseType::Sqlite, &dir.path().join("source.db"));
        let target = file_profile(DatabaseType::DuckDb, &dir.path().join("target.duckdb"));

        let sqlite = sqlly_engines::sqlite::open_client(&source).await.unwrap();
        for statement in [
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, price REAL, note TEXT, active INTEGER)",
            "INSERT INTO items VALUES (1, 'apple', 1.25, NULL, 1), (2, 'pear', 2.5, 'ripe', 0), (3, 'o''brien', NULL, 'quote', 1)",
        ] {
            sqlly_engines::sqlite::execute_params_on_client(&sqlite, statement, &[])
                .await
                .unwrap();
        }
        drop(sqlite);

        let plan = TransferPlan {
            source: DatabaseType::Sqlite,
            target: DatabaseType::DuckDb,
            target_schema: Some("main".into()),
            mode: TransferMode::CreateTable,
            casing: NameCasing::AsIs,
            tables: vec![TransferTable {
                schema: None,
                name: "items".into(),
                columns: vec![
                    TransferColumn {
                        name: "id".into(),
                        source_type: "INTEGER".into(),
                        nullable: false,
                        is_identity: false,
                        pk_ordinal: Some(1),
                    },
                    TransferColumn {
                        name: "name".into(),
                        source_type: "TEXT".into(),
                        nullable: false,
                        is_identity: false,
                        pk_ordinal: None,
                    },
                    TransferColumn {
                        name: "price".into(),
                        source_type: "REAL".into(),
                        nullable: true,
                        is_identity: false,
                        pk_ordinal: None,
                    },
                    TransferColumn {
                        name: "note".into(),
                        source_type: "TEXT".into(),
                        nullable: true,
                        is_identity: false,
                        pk_ordinal: None,
                    },
                    TransferColumn {
                        name: "active".into(),
                        source_type: "INTEGER".into(),
                        nullable: true,
                        is_identity: false,
                        pk_ordinal: None,
                    },
                ],
                depends_on: vec![],
            }],
        };

        let services = crate::services::Services::new();
        let mut sink = open_sink(&target).await.unwrap();
        assert_eq!(sink.database_type(), DatabaseType::DuckDb);
        let (progress, mut events) = tokio::sync::mpsc::channel(64);
        let outcome = run_transfer(
            &services,
            &source,
            &mut sink,
            &plan,
            progress,
            TransferCancel::default(),
        )
        .await;
        assert_eq!(outcome.error, None, "{outcome:?}");
        assert!(!outcome.cancelled);
        assert_eq!(outcome.tables, vec![("items".to_string(), 3)]);
        let mut finished = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, crate::transfer::run::TransferProgress::Finished(_)) {
                finished = true;
            }
        }
        assert!(finished);
        drop(sink);

        let duck = sqlly_engines::duckdb::open_client(&target).await.unwrap();
        let sets = sqlly_engines::duckdb::query_rows_on_client(
            &duck,
            "SELECT id, name, price, note, active FROM main.items ORDER BY id",
            &[],
        )
        .await
        .unwrap();
        let rows = &sets[0].rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1], CellValue::String("apple".into()));
        assert_eq!(rows[0][2], CellValue::F64(1.25));
        assert_eq!(rows[0][3], CellValue::Null);
        assert_eq!(rows[1][3], CellValue::String("ripe".into()));
        assert_eq!(rows[2][1], CellValue::String("o'brien".into()));
        assert_eq!(rows[2][2], CellValue::Null);
        assert_eq!(rows[2][4], CellValue::I64(1));

        // The created table carries the primary key the plan declared.
        let sets = sqlly_engines::duckdb::query_rows_on_client(
            &duck,
            "SELECT constraint_type FROM duckdb_constraints() WHERE table_name = 'items' AND constraint_type = 'PRIMARY KEY'",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(sets[0].rows.len(), 1);
    }

    /// A failing statement rolls the DuckDB sink's transaction back so the
    /// table is left untouched.
    #[tokio::test]
    async fn duckdb_sink_rolls_back_a_failed_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let target = file_profile(DatabaseType::DuckDb, &dir.path().join("rollback.duckdb"));
        let mut sink = open_sink(&target).await.unwrap();
        sink.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
            .await
            .unwrap();
        sink.begin().await.unwrap();
        assert_eq!(
            sink.execute("INSERT INTO t VALUES (?)", &[SqlParam::I64(1)])
                .await
                .unwrap(),
            1
        );
        assert!(
            sink.execute("INSERT INTO t VALUES (?)", &[SqlParam::I64(1)])
                .await
                .is_err(),
            "duplicate key"
        );
        sink.rollback().await.unwrap();
        drop(sink);
        let duck = sqlly_engines::duckdb::open_client(&target).await.unwrap();
        let sets =
            sqlly_engines::duckdb::query_rows_on_client(&duck, "SELECT COUNT(*) FROM t", &[])
                .await
                .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(0));
    }
}

/// Test double that records statements and transaction events.
#[derive(Debug, Default)]
pub struct RecordingSink {
    pub db: DatabaseType,
    pub statements: Vec<(String, usize)>,
    pub tx_events: Vec<&'static str>,
    pub fail_on_statement: Option<usize>,
}

#[async_trait]
impl TransferSink for RecordingSink {
    fn database_type(&self) -> DatabaseType {
        self.db
    }

    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> StudioResult<u64> {
        let index = self.statements.len() + 1;
        if self.fail_on_statement == Some(index) {
            return Err(StudioError::QueryExecution("injected sink failure".into()));
        }
        self.statements.push((sql.to_string(), params.len()));
        Ok(params.len() as u64)
    }

    async fn begin(&mut self) -> StudioResult<()> {
        self.tx_events.push("begin");
        Ok(())
    }

    async fn commit(&mut self) -> StudioResult<()> {
        self.tx_events.push("commit");
        Ok(())
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        self.tx_events.push("rollback");
        Ok(())
    }
}
