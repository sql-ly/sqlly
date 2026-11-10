//! # sqlly-sqlite
//!
//! SQLite driver mirroring the `sqlly-tds` API surface: [`open_client`] /
//! [`execute_on_client`] for the streaming query path, plus
//! [`query_rows_on_client`] / [`execute_params_on_client`] for parameterized
//! reads and writes, and [`SqliteTransport`] implementing
//! `crate::driver::SqlTransport` for the lifecycle service.
//!
//! rusqlite is synchronous, so each [`SqliteClient`] owns a dedicated OS
//! thread that holds the `rusqlite::Connection` for its whole life; async
//! callers talk to it over channels. Query results stream back item-by-item
//! through a bounded channel (`blocking_send` on the thread side gives real
//! backpressure), and [`execute_on_client`] pumps those items through the
//! shared [`crate::driver::QueryStreamer`], so the event/row contract is
//! identical to the TDS driver's. Dropping the client (or the item receiver,
//! on cancellation) stops the thread at the next statement/row boundary.

#![forbid(unsafe_code)]

use crate::driver::QueryStreamer;
use rusqlite::types::ValueRef;
use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseSummary, EventBus, MaterializedResultSet, ResultColumn,
    RowSender, SqlParam, StudioError, StudioResult,
};
use tokio::sync::{mpsc, oneshot};

pub use crate::driver::{ConnectionService, SqlTransport};

/// Items streamed from the connection thread while a statement batch runs.
#[derive(Debug)]
enum Item {
    ResultSet(Vec<ResultColumn>),
    Row(Vec<CellValue>),
    /// A statement that produced no result set, with its affected-row count.
    Affected(u64),
    Error(String),
}

enum Request {
    Execute {
        sql: String,
        items: mpsc::Sender<Item>,
    },
    QueryRows {
        sql: String,
        params: Vec<SqlParam>,
        respond: oneshot::Sender<Result<Vec<MaterializedResultSet>, String>>,
    },
    ExecuteParams {
        sql: String,
        params: Vec<SqlParam>,
        respond: oneshot::Sender<Result<u64, String>>,
    },
    Describe {
        sql: String,
        respond: oneshot::Sender<Result<Vec<SqliteColumnOrigin>, String>>,
    },
}

/// Origin metadata for one column of a prepared — never executed — statement.
/// SQLite's `sqlite3_column_{database,table,origin}_name` report the attached
/// database, base table, and base column each result column derives from;
/// all three are `None` for expressions. This is the SQLite equivalent of
/// `sp_describe_first_result_set` provenance: the engine itself reports where
/// each projected column comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteColumnOrigin {
    pub name: String,
    pub database: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
}

/// A live SQLite "connection": a dedicated thread owning the
/// `rusqlite::Connection`, reachable over a request channel. Dropping the
/// client closes the channel and ends the thread.
pub struct SqliteClient {
    requests: mpsc::Sender<Request>,
    interrupt: std::sync::Arc<rusqlite::InterruptHandle>,
}

/// Cloneable server-side interruption handle: stops the connection's
/// currently executing statement at the next VM step (rusqlite
/// `interrupt()`), which surfaces as a query error on the running batch.
#[derive(Clone)]
pub struct SqliteInterruptHandle(std::sync::Arc<rusqlite::InterruptHandle>);

impl SqliteInterruptHandle {
    pub fn interrupt(&self) {
        self.0.interrupt();
    }
}

/// Obtain the interruption handle for a client's connection.
pub fn interrupt_handle(client: &SqliteClient) -> SqliteInterruptHandle {
    SqliteInterruptHandle(std::sync::Arc::clone(&client.interrupt))
}

/// Resolve the database file path from a profile: `database` wins, then
/// `server` (the connection editor's single field for file databases).
/// `:memory:` is honored. When `server` holds the file, a bare `database`
/// word (no path separator or extension, such as the explorer's `main` node
/// or a `master` default) is a schema name, not a file, and resolves to the
/// profile's file instead of creating `./main`.
pub fn database_path(profile: &ConnectionProfile) -> StudioResult<String> {
    let has_server_file = !profile.server.trim().is_empty();
    let path = profile
        .database
        .as_deref()
        .map(str::trim)
        .filter(|s| {
            let bare_word = !s.contains(['/', '\\', '.', ':']);
            !s.is_empty() && !(has_server_file && bare_word)
        })
        .or_else(|| {
            let s = profile.server.trim();
            if s.is_empty() { None } else { Some(s) }
        });
    path.map(str::to_string).ok_or_else(|| {
        StudioError::InvalidProfile(
            "SQLite profile needs a file path (or :memory:) in database/server".to_string(),
        )
    })
}

/// Open a client for the profile's database file. Fails fast if the file
/// cannot be opened (bad path, locked, not a database).
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<SqliteClient> {
    let path = database_path(profile)?;
    let (tx, rx) = mpsc::channel::<Request>(16);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<rusqlite::InterruptHandle, String>>();
    std::thread::Builder::new()
        .name("sqlly-sqlite".to_string())
        .spawn(move || connection_thread(path, rx, ready_tx))
        .map_err(|e| StudioError::Connection(format!("failed to spawn sqlite thread: {e}")))?;
    let interrupt = ready_rx
        .await
        .map_err(|_| StudioError::Connection("sqlite thread exited before ready".to_string()))?
        .map_err(StudioError::Connection)?;
    let client = SqliteClient {
        requests: tx,
        interrupt: std::sync::Arc::new(interrupt),
    };
    apply_startup_sql(&client, profile).await?;
    Ok(client)
}

/// Run a raw statement batch with no safety policy and no result decoding.
pub async fn execute_batch_on_client(client: &SqliteClient, sql: &str) -> StudioResult<()> {
    let (items_tx, mut items_rx) = mpsc::channel::<Item>(16);
    client
        .requests
        .send(Request::Execute {
            sql: sql.to_string(),
            items: items_tx,
        })
        .await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?;
    while let Some(item) = items_rx.recv().await {
        if let Item::Error(message) = item {
            return Err(StudioError::QueryExecution(message));
        }
    }
    Ok(())
}

async fn apply_startup_sql(client: &SqliteClient, profile: &ConnectionProfile) -> StudioResult<()> {
    let Some(batches) = sqlly_core::startup_sql_batches(profile) else {
        return Ok(());
    };
    for batch in batches {
        execute_batch_on_client(client, batch)
            .await
            .map_err(sqlly_core::startup_sql_failed)?;
    }
    Ok(())
}

/// Run a statement batch on an open client, streaming results through the
/// shared driver contract (events on the bus, rows on the bounded channel).
/// Applies the profile's safety policy with SQLite transaction syntax.
///
/// A profile carrying `enforce_read_only` runs the batch with
/// `PRAGMA query_only = 1` switched on for this connection, so the engine
/// itself refuses any write the SELECT-only parser gate could not see. The
/// pragma is restored afterwards whether the batch succeeded or failed, so a
/// shared or pooled connection is never left query-only. (If the awaiting
/// task is aborted mid-stream the restore cannot run; the producer drops the
/// client in that case, as it does for every aborted run.)
pub async fn execute_on_client(
    client: &SqliteClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    let sql = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    if !profile.enforce_read_only {
        return stream_batch(client, sql, events, rows_tx).await;
    }
    execute_batch_on_client(client, "PRAGMA query_only = 1").await?;
    let result = stream_batch(client, sql, events, rows_tx).await;
    let restored = execute_batch_on_client(client, "PRAGMA query_only = 0").await;
    match (result, restored) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// The streaming body of [`execute_on_client`]: the policied SQL is already
/// in hand, so no policy is applied here.
async fn stream_batch(
    client: &SqliteClient,
    sql: String,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    let (items_tx, mut items_rx) = mpsc::channel::<Item>(256);
    client
        .requests
        .send(Request::Execute {
            sql,
            items: items_tx,
        })
        .await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?;

    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    let mut failure: Option<StudioError> = None;
    while let Some(item) = items_rx.recv().await {
        match item {
            Item::ResultSet(columns) => streamer.begin_result_set(columns).await?,
            Item::Row(cells) => {
                if !streamer.push_row(cells).await? {
                    // Consumer gone: drop the receiver so the connection
                    // thread stops producing, then stop draining.
                    break;
                }
            }
            Item::Affected(n) => streamer.statement_completed(n)?,
            Item::Error(message) => {
                failure = Some(StudioError::QueryExecution(message));
                break;
            }
        }
    }
    drop(items_rx);
    match failure {
        Some(error) => Err(error),
        None => streamer.finish().await,
    }
}

/// Parameterized, materialized read on an open client (`?` placeholders).
/// Mirrors `crate::tds::query_rows_on_client`: small targeted reads, no safety
/// policy applied.
pub async fn query_rows_on_client(
    client: &SqliteClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<Vec<MaterializedResultSet>> {
    let (tx, rx) = oneshot::channel();
    client
        .requests
        .send(Request::QueryRows {
            sql: sql.to_string(),
            params: params.to_vec(),
            respond: tx,
        })
        .await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// Parameterized non-query on an open client; returns affected rows.
pub async fn execute_params_on_client(
    client: &SqliteClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<u64> {
    let (tx, rx) = oneshot::channel();
    client
        .requests
        .send(Request::ExecuteParams {
            sql: sql.to_string(),
            params: params.to_vec(),
            respond: tx,
        })
        .await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// Prepare `sql` on the client's connection and return each result column's
/// origin. Only the first statement of a batch is described (mirroring
/// "first result set" provenance); nothing is executed.
pub async fn describe_statement_on_client(
    client: &SqliteClient,
    sql: &str,
) -> StudioResult<Vec<SqliteColumnOrigin>> {
    let (tx, rx) = oneshot::channel();
    client
        .requests
        .send(Request::Describe {
            sql: sql.to_string(),
            respond: tx,
        })
        .await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("sqlite connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// `SqlTransport` implementation for SQLite, usable with the shared
/// [`ConnectionService`].
#[derive(Debug, Clone, Default)]
pub struct SqliteTransport;

#[async_trait::async_trait]
impl SqlTransport for SqliteTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        let sets = query_rows_on_client(&client, "PRAGMA database_list", &[]).await?;
        let mut out = Vec::new();
        for set in sets {
            for row in set.rows {
                // PRAGMA database_list: seq, name, file.
                if let Some(CellValue::String(name)) = row.get(1) {
                    out.push(DatabaseSummary {
                        name: name.clone(),
                        state: "ONLINE".to_string(),
                        compatibility_level: None,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()> {
        let client = open_client(profile).await?;
        execute_on_client(&client, profile, sql, events, rows_tx).await
    }
}

// ── Connection thread ────────────────────────────────────────────────────

fn connection_thread(
    path: String,
    mut requests: mpsc::Receiver<Request>,
    ready: oneshot::Sender<Result<rusqlite::InterruptHandle, String>>,
) {
    let conn = match rusqlite::Connection::open(&path) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = ready.send(Err(format!("cannot open '{path}': {e}")));
            return;
        }
    };
    // Probe: `Connection::open` succeeds on any path; a bad database file
    // only fails on first use.
    if let Err(e) = conn.query_row("select 1", [], |_| Ok(())) {
        let _ = ready.send(Err(format!("'{path}' is not a usable database: {e}")));
        return;
    }
    let _ = ready.send(Ok(conn.get_interrupt_handle()));

    while let Some(request) = requests.blocking_recv() {
        match request {
            Request::Execute { sql, items } => run_batch(&conn, &sql, &items),
            Request::QueryRows {
                sql,
                params,
                respond,
            } => {
                let _ = respond.send(run_query_rows(&conn, &sql, &params));
            }
            Request::ExecuteParams {
                sql,
                params,
                respond,
            } => {
                let _ = respond.send(run_execute_params(&conn, &sql, &params));
            }
            Request::Describe { sql, respond } => {
                let _ = respond.send(run_describe(&conn, &sql));
            }
        }
    }
}

fn run_describe(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<SqliteColumnOrigin>, String> {
    let stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    Ok(stmt
        .columns_with_metadata()
        .map(|meta| SqliteColumnOrigin {
            name: meta.name().to_string(),
            database: meta.database_name().map(str::to_string),
            table: meta.table_name().map(str::to_string),
            column: meta.origin_name().map(str::to_string),
        })
        .collect())
}

/// Run a multi-statement batch, streaming items. A send failure means the
/// async side hung up (cancellation) — stop quietly.
fn run_batch(conn: &rusqlite::Connection, sql: &str, items: &mpsc::Sender<Item>) {
    use fallible_iterator::FallibleIterator;
    let mut batch = rusqlite::Batch::new(conn, sql);
    loop {
        let mut stmt = match batch.next() {
            Ok(Some(stmt)) => stmt,
            Ok(None) => return,
            Err(e) => {
                let _ = items.blocking_send(Item::Error(e.to_string()));
                return;
            }
        };
        if stmt.column_count() == 0 {
            match stmt.execute([]) {
                Ok(changed) => {
                    if items.blocking_send(Item::Affected(changed as u64)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = items.blocking_send(Item::Error(e.to_string()));
                    return;
                }
            }
            continue;
        }

        let columns: Vec<ResultColumn> = stmt
            .columns()
            .map(|c| ResultColumn {
                name: c.name().to_string(),
                data_type: c.decl_type().unwrap_or("").to_lowercase(),
                nullable: true,
            })
            .collect();
        let column_count = columns.len();
        if items.blocking_send(Item::ResultSet(columns)).is_err() {
            return;
        }

        let mut rows = match stmt.query([]) {
            Ok(rows) => rows,
            Err(e) => {
                let _ = items.blocking_send(Item::Error(e.to_string()));
                return;
            }
        };
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    let cells = (0..column_count)
                        .map(|i| value_ref_to_cell(row.get_ref(i)))
                        .collect();
                    if items.blocking_send(Item::Row(cells)).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = items.blocking_send(Item::Error(e.to_string()));
                    return;
                }
            }
        }
    }
}

fn run_query_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[SqlParam],
) -> Result<Vec<MaterializedResultSet>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let columns: Vec<ResultColumn> = stmt
        .columns()
        .map(|c| ResultColumn {
            name: c.name().to_string(),
            data_type: c.decl_type().unwrap_or("").to_lowercase(),
            nullable: true,
        })
        .collect();
    let column_count = columns.len();
    let values = params.iter().map(param_to_value).collect::<Vec<_>>();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(values))
        .map_err(|e| e.to_string())?;
    let mut out_rows = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => out_rows.push(
                (0..column_count)
                    .map(|i| value_ref_to_cell(row.get_ref(i)))
                    .collect(),
            ),
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(vec![MaterializedResultSet {
        columns,
        rows: out_rows,
    }])
}

fn run_execute_params(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[SqlParam],
) -> Result<u64, String> {
    let values = params.iter().map(param_to_value).collect::<Vec<_>>();
    conn.execute(sql, rusqlite::params_from_iter(values))
        .map(|n| n as u64)
        .map_err(|e| e.to_string())
}

fn param_to_value(param: &SqlParam) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match param {
        SqlParam::Null => Value::Null,
        SqlParam::Bool(b) => Value::Integer(i64::from(*b)),
        SqlParam::I64(n) => Value::Integer(*n),
        SqlParam::F64(f) => Value::Real(*f),
        SqlParam::String(s) => Value::Text(s.clone()),
        SqlParam::Bytes(b) => Value::Blob(b.clone()),
    }
}

fn value_ref_to_cell(value: Result<ValueRef<'_>, rusqlite::Error>) -> CellValue {
    match value {
        Ok(ValueRef::Null) => CellValue::Null,
        Ok(ValueRef::Integer(n)) => CellValue::I64(n),
        Ok(ValueRef::Real(f)) => CellValue::F64(f),
        Ok(ValueRef::Text(bytes)) => match std::str::from_utf8(bytes) {
            Ok(text) => CellValue::String(text.to_string()),
            Err(_) => CellValue::Bytes(bytes.to_vec()),
        },
        Ok(ValueRef::Blob(bytes)) => CellValue::Bytes(bytes.to_vec()),
        Err(e) => CellValue::String(format!("<error: {e}>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{EventPayload, SecretProvider, SecretRef, row_channel};

    fn memory_profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "sqlite",
            ":memory:",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Sqlite;
        p
    }

    fn payloads(sub: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
        let mut out = Vec::new();
        while let Ok(event) = sub.try_recv() {
            out.push(event.payload);
        }
        out
    }

    #[test]
    fn database_path_prefers_database_then_server() {
        let mut p = memory_profile();
        assert_eq!(database_path(&p).unwrap(), ":memory:");
        p.database = Some("C:/tmp/x.db".into());
        assert_eq!(database_path(&p).unwrap(), "C:/tmp/x.db");
        p.database = Some("  ".into());
        p.server = String::new();
        assert!(database_path(&p).is_err());
    }

    /// The explorer scopes metadata to the tree's database node, which on
    /// SQLite is the schema name `main` (or `temp`, or IntelliSense's `master`
    /// default), not a file: it must
    /// resolve to the profile's own file instead of creating `./main`.
    #[test]
    fn database_path_treats_bare_schema_names_as_the_profile_file() {
        let mut p = memory_profile();
        p.server = "/data/shop.db".into();
        for schema in ["main", "temp", "MAIN", "master"] {
            p.database = Some(schema.into());
            assert_eq!(database_path(&p).unwrap(), "/data/shop.db");
        }
        // A real file in `database` still wins, as does `:memory:`.
        p.database = Some("other.db".into());
        assert_eq!(database_path(&p).unwrap(), "other.db");
        p.database = Some(":memory:".into());
        assert_eq!(database_path(&p).unwrap(), ":memory:");
        // With no file in `server`, a bare name is all there is.
        p.server = String::new();
        p.database = Some("shop".into());
        assert_eq!(database_path(&p).unwrap(), "shop");
    }

    #[tokio::test]
    async fn open_client_rejects_unusable_paths() {
        let mut p = memory_profile();
        p.server = "Z:/definitely/not/a/dir/x.db".into();
        assert!(open_client(&p).await.is_err());
    }

    #[tokio::test]
    async fn execute_streams_full_tds_contract() {
        let profile = memory_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(256);
        let mut sub = events.subscribe();
        let (tx, mut rx) = row_channel(64);

        execute_on_client(
            &client,
            &profile,
            "CREATE TABLE t (id INTEGER, name TEXT);\n\
             INSERT INTO t VALUES (1, 'a'), (2, 'b');\n\
             SELECT id, name FROM t ORDER BY id;",
            &events,
            Some(&tx),
        )
        .await
        .unwrap();
        drop(tx);

        let seen = payloads(&mut sub);
        assert!(matches!(seen[0], EventPayload::QueryStarted { .. }));
        // CREATE affected 0, INSERT affected 2.
        assert!(seen.iter().any(|p| matches!(
            p,
            EventPayload::RowCount {
                rows_affected: 2,
                ..
            }
        )));
        let columns = seen.iter().find_map(|p| match p {
            EventPayload::ResultSetStarted { columns, .. } => Some(columns.clone()),
            _ => None,
        });
        let columns = columns.expect("result set started");
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].data_type, "integer");
        assert!(matches!(
            seen.last().unwrap(),
            EventPayload::QueryCompleted { .. }
        ));

        let batch = rx.recv().await.expect("rows batch");
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.rows[0][0], CellValue::I64(1));
        assert_eq!(batch.rows[1][1], CellValue::String("b".into()));
    }

    #[tokio::test]
    async fn execute_reports_sql_errors() {
        let profile = memory_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let result = execute_on_client(&client, &profile, "SELEC nope", &events, None).await;
        assert!(matches!(result, Err(StudioError::QueryExecution(_))));
    }

    #[tokio::test]
    async fn select_only_policy_blocks_mutations() {
        let mut profile = memory_profile();
        profile.block_all_but_select = true;
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let result = execute_on_client(&client, &profile, "DROP TABLE t", &events, None).await;
        assert!(matches!(result, Err(StudioError::PermissionDenied(_))));
    }

    #[tokio::test]
    async fn enforced_read_only_is_refused_by_the_engine_and_restored_after() {
        let mut enforced = memory_profile();
        enforced.enforce_read_only = true;
        // No parser gate here on purpose: this proves the ENGINE refuses the
        // write under `PRAGMA query_only`, not the classifier.
        enforced.block_all_but_select = false;
        let client = open_client(&enforced).await.unwrap();
        let events = EventBus::new(256);
        let _sub = events.subscribe();

        let plain = memory_profile();
        execute_on_client(
            &client,
            &plain,
            "CREATE TABLE t (id INTEGER);",
            &events,
            None,
        )
        .await
        .unwrap();

        // A SELECT still works under the read-only mode.
        execute_on_client(&client, &enforced, "SELECT id FROM t", &events, None)
            .await
            .unwrap();

        // A write is refused by SQLite itself, in words the hosts recognise.
        let error = execute_on_client(
            &client,
            &enforced,
            "INSERT INTO t VALUES (1)",
            &events,
            None,
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(
            sqlly_intel::query::safety::is_read_only_refusal(
                sqlly_core::DatabaseType::Sqlite,
                &message
            ),
            "{message}"
        );

        // The failed run must have restored the session: the same connection
        // accepts a write again once the profile no longer enforces read-only.
        execute_on_client(&client, &plain, "INSERT INTO t VALUES (2)", &events, None)
            .await
            .unwrap();
        let sets = query_rows_on_client(&client, "SELECT COUNT(*) FROM t", &[])
            .await
            .unwrap();
        assert_eq!(
            sets[0].rows[0][0],
            CellValue::I64(1),
            "only the plain insert landed"
        );
    }

    #[tokio::test]
    async fn rollback_policy_leaves_no_trace() {
        let mut profile = memory_profile();
        profile.auto_wrap_rollback = true;
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(256);
        let _sub = events.subscribe();

        // Setup without the wrap.
        let mut plain = profile.clone();
        plain.auto_wrap_rollback = false;
        execute_on_client(
            &client,
            &plain,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);",
            &events,
            None,
        )
        .await
        .unwrap();

        // Wrapped delete rolls back.
        execute_on_client(&client, &profile, "DELETE FROM t;", &events, None)
            .await
            .unwrap();
        let sets = query_rows_on_client(&client, "SELECT COUNT(*) FROM t", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(1), "delete rolled back");
    }

    #[tokio::test]
    async fn query_rows_binds_parameters() {
        let profile = memory_profile();
        let client = open_client(&profile).await.unwrap();
        execute_params_on_client(&client, "CREATE TABLE p (id INTEGER, name TEXT)", &[])
            .await
            .unwrap();
        let inserted = execute_params_on_client(
            &client,
            "INSERT INTO p VALUES (?, ?), (?, ?)",
            &[
                SqlParam::I64(1),
                SqlParam::String("alpha".into()),
                SqlParam::I64(2),
                SqlParam::String("beta".into()),
            ],
        )
        .await
        .unwrap();
        assert_eq!(inserted, 2);

        let sets = query_rows_on_client(
            &client,
            "SELECT name FROM p WHERE id = ?",
            &[SqlParam::I64(2)],
        )
        .await
        .unwrap();
        assert_eq!(sets[0].rows, vec![vec![CellValue::String("beta".into())]]);
    }

    #[tokio::test]
    async fn value_mapping_covers_all_storage_classes() {
        let profile = memory_profile();
        let client = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&client, "SELECT NULL, 42, 1.5, 'text', x'DEAD'", &[])
            .await
            .unwrap();
        let row = &sets[0].rows[0];
        assert_eq!(row[0], CellValue::Null);
        assert_eq!(row[1], CellValue::I64(42));
        assert_eq!(row[2], CellValue::F64(1.5));
        assert_eq!(row[3], CellValue::String("text".into()));
        assert_eq!(row[4], CellValue::Bytes(vec![0xDE, 0xAD]));
    }

    #[tokio::test]
    async fn invalid_utf8_text_is_preserved_as_bytes() {
        let profile = memory_profile();
        let client = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&client, "SELECT CAST(X'80' AS TEXT) AS bad", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::Bytes(vec![0x80]));
    }

    #[tokio::test]
    async fn valid_utf8_text_stays_string() {
        let profile = memory_profile();
        let client = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&client, "SELECT 'hello' AS good", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::String("hello".into()));
    }

    #[tokio::test]
    async fn transport_lists_databases_and_tests_connection() {
        let profile = memory_profile();
        let transport = SqliteTransport;
        transport.test_connection(&profile).await.unwrap();
        let dbs = transport.list_databases(&profile).await.unwrap();
        assert_eq!(dbs[0].name, "main");
    }

    #[tokio::test]
    async fn multiple_result_sets_are_indexed_in_order() {
        let profile = memory_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(256);
        let mut sub = events.subscribe();
        let (tx, mut rx) = row_channel(64);
        execute_on_client(
            &client,
            &profile,
            "SELECT 1 AS a; SELECT 'x' AS b, 'y' AS c;",
            &events,
            Some(&tx),
        )
        .await
        .unwrap();
        drop(tx);

        let indices: Vec<usize> = payloads(&mut sub)
            .iter()
            .filter_map(|p| match p {
                EventPayload::ResultSetStarted {
                    result_set_index, ..
                } => Some(*result_set_index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![0, 1]);
        let b1 = rx.recv().await.unwrap();
        let b2 = rx.recv().await.unwrap();
        assert_eq!((b1.result_set_index, b2.result_set_index), (0, 1));
    }

    #[tokio::test]
    async fn open_client_runs_startup_sql() {
        let mut profile = memory_profile();
        profile.startup_sql = Some(
            "CREATE TABLE startup_probe (id INTEGER); INSERT INTO startup_probe VALUES (1);"
                .to_string(),
        );
        let client = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&client, "SELECT id FROM startup_probe", &[])
            .await
            .unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].rows[0][0], CellValue::I64(1));
    }

    #[tokio::test]
    async fn open_client_fails_when_startup_sql_errors() {
        let mut profile = memory_profile();
        profile.startup_sql = Some("SELECT no_such_table;".to_string());
        let err = open_client(&profile).await;
        assert!(matches!(
            err,
            Err(StudioError::Connection(msg)) if msg.starts_with("Startup SQL failed:")
        ));
    }
}
