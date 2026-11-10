//! # Oracle Database driver
//!
//! Oracle through ODPI-C (the `oracle` crate), mirroring the DuckDB driver's
//! shape: [`open_client`] / [`execute_on_client`] for the streaming query
//! path, [`query_rows_on_client`] / [`execute_params_on_client`] for
//! parameterized reads and writes, and [`OracleTransport`] implementing
//! `crate::driver::SqlTransport` for the lifecycle service.
//!
//! The client library is synchronous, so each [`OracleClient`] owns a
//! dedicated OS thread that holds the `oracle::Connection` for its whole
//! life; async callers talk to it over channels. Rows stream back through a
//! bounded channel (`blocking_send` on the thread side gives real
//! backpressure) and [`execute_on_client`] pumps them through the shared
//! [`crate::driver::QueryStreamer`], so the event/row contract is identical
//! to the other drivers'. Cancel is `Connection::break_execution`
//! (`dpiConn_breakExecution`): the running statement fails with ORA-01013 and
//! the batch ends there.
//!
//! Scripts are split by [`script::split_script`], which knows that a PL/SQL
//! unit keeps its inner `;` and ends at `/` (or its outermost `END;`), while
//! plain SQL must reach OCI without a terminator. `DBMS_OUTPUT` is enabled
//! on every connection and drained after each statement into `Message`
//! events, so `PUT_LINE` output shows up in the Messages pane.
//!
//! The ODPI-C library needs Oracle's free Instant Client at runtime; when it
//! cannot be loaded every connect fails with one actionable message (see
//! [`connect::client_library_missing_message`]) that names the library, the
//! search paths, and the [`connect::CLIENT_DIR_ENV`] override.

#![forbid(unsafe_code)]

pub mod connect;
pub mod script;
pub mod values;

use crate::driver::QueryStreamer;
use oracle::sql_type::{OracleType, ToSql};
use oracle::{Connection, Connector, Privilege};
use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseSummary, EventBus, MaterializedResultSet, ResultColumn,
    RowSender, SqlParam, StudioError, StudioResult,
};
use std::sync::{Arc, Once};
use tokio::sync::{mpsc, oneshot};

pub use crate::driver::{ConnectionService, SqlTransport};
pub use connect::{
    CLIENT_DIR_ENV, SID_PREFIX, SessionRole, client_library_missing_message, connect_string,
    connect_string_at, is_client_library_missing, strip_sid_prefix, uses_plain_endpoint,
};
pub use script::{ScriptStatement, split_script};
pub use values::{FetchKind, fetch_kind, number_cell};

/// Rows fetched per round trip while streaming a result set.
const FETCH_ARRAY_SIZE: u32 = 256;

/// Upper bound on `DBMS_OUTPUT` drain round trips per statement (each one
/// moves up to 32 KB), so a runaway loop cannot stall the batch forever.
const MAX_OUTPUT_DRAINS: usize = 4096;

/// Items streamed from the connection thread while a statement batch runs.
#[derive(Debug)]
enum Item {
    ResultSet(Vec<ResultColumn>),
    Row(Vec<CellValue>),
    /// A statement that produced no result set, with its affected-row count.
    Affected(u64),
    /// One `DBMS_OUTPUT` line (or engine notice).
    Message(String),
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
    ExecuteReturningText {
        sql: String,
        params: Vec<SqlParam>,
        out_name: String,
        respond: oneshot::Sender<Result<(u64, Option<String>), String>>,
    },
}

/// A live Oracle session: a dedicated thread owning the `oracle::Connection`,
/// reachable over a request channel. Dropping the client closes the channel,
/// which ends the thread and closes the session.
pub struct OracleClient {
    requests: mpsc::Sender<Request>,
    conn: Arc<Connection>,
}

impl std::fmt::Debug for OracleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OracleClient")
            .field("open", &!self.requests.is_closed())
            .finish()
    }
}

/// Cloneable cancellation handle: interrupts the session's currently
/// executing statement (`dpiConn_breakExecution`), which surfaces as an
/// ORA-01013 error on the running batch.
#[derive(Clone)]
pub struct OracleBreakHandle(Arc<Connection>);

impl OracleBreakHandle {
    pub fn break_execution(&self) {
        if let Err(error) = self.0.break_execution() {
            tracing::debug!(%error, "oracle break_execution failed");
        }
    }
}

/// Obtain the cancellation handle for a client's session.
pub fn break_handle(client: &OracleClient) -> OracleBreakHandle {
    OracleBreakHandle(Arc::clone(&client.conn))
}

/// Point ODPI-C at an explicit Instant Client folder when the user set
/// [`CLIENT_DIR_ENV`]. Runs once per process, before the first connection;
/// without the variable the library's own search order applies.
fn init_client_library() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let Ok(dir) = std::env::var(CLIENT_DIR_ENV) else {
            return;
        };
        let dir = dir.trim().to_string();
        if dir.is_empty() {
            return;
        }
        let mut params = oracle::InitParams::new();
        let result = params
            .oracle_client_lib_dir(dir.clone())
            .and_then(|p| p.init());
        if let Err(error) = result {
            tracing::warn!(%error, dir, "SQLLY_ORACLE_CLIENT_DIR could not be applied");
        }
    });
}

/// Whether the Oracle Instant Client can be loaded on this machine, without
/// dialing any server: `Ok(version)` names the client library version that
/// was found (for example `23.3.0.0.0`), `Err(message)` carries the same
/// actionable text a failed connect would show. Honors
/// [`CLIENT_DIR_ENV`]. Used by `sqlly-cli doctor`.
pub fn client_library_version() -> Result<String, String> {
    init_client_library();
    match oracle::Version::client() {
        Ok(version) => Ok(version.to_string()),
        Err(error) if connect::is_client_library_missing(&error) => {
            Err(client_library_missing_message(&error.to_string()))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Open a session for the profile. Fails fast with the Instant Client
/// instructions when the client library is missing, an authentication error
/// on ORA-01017, and Oracle's own text for anything else (no listener,
/// unknown service, …).
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<OracleClient> {
    // Under a rollback wrap or the read-only screen the policy appends
    // `ROLLBACK`, which only means something when statements are not
    // committed as they run; everywhere else Oracle behaves like the other
    // network engines (each statement commits).
    let autocommit = !(profile.auto_wrap_rollback || profile.enforce_read_only);
    open_client_with_autocommit(profile, autocommit).await
}

/// Open a session that never commits on its own: the row-editing lane and
/// the data-transfer sink run explicit transactions (`COMMIT` / `ROLLBACK`
/// statements) and must not have each statement committed as it runs.
pub async fn open_client_manual_commit(profile: &ConnectionProfile) -> StudioResult<OracleClient> {
    open_client_with_autocommit(profile, false).await
}

/// The local endpoint an SSH tunnel, proxy hop or Kubernetes port-forward
/// exposes for the profile, or `None` when the session dials the listener
/// directly: no transport is configured, or the Server field is a connect
/// descriptor, EZConnect string or `tnsnames.ora` alias whose address the
/// client resolves itself (those stay untunnelled).
async fn tunnelled_endpoint(profile: &ConnectionProfile) -> StudioResult<Option<(String, u16)>> {
    if profile.inline_transport().is_empty() || !connect::uses_plain_endpoint(profile) {
        return Ok(None);
    }
    let (host, port) = crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    Ok(Some((host, port)))
}

async fn open_client_with_autocommit(
    profile: &ConnectionProfile,
    autocommit: bool,
) -> StudioResult<OracleClient> {
    let endpoint = tunnelled_endpoint(profile).await?;
    let (params, password) =
        connect::connect_params_at(profile, endpoint.as_ref().map(|(h, p)| (h.as_str(), *p)))?;
    let (tx, rx) = mpsc::channel::<Request>(16);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<Arc<Connection>, StudioError>>();
    std::thread::Builder::new()
        .name("sqlly-oracle".to_string())
        .spawn(move || connection_thread(params, password, autocommit, rx, ready_tx))
        .map_err(|e| StudioError::Connection(format!("failed to spawn oracle thread: {e}")))?;
    let conn = ready_rx
        .await
        .map_err(|_| StudioError::Connection("oracle thread exited before ready".to_string()))??;
    let client = OracleClient { requests: tx, conn };
    apply_startup_sql(&client, profile).await?;
    Ok(client)
}

async fn apply_startup_sql(client: &OracleClient, profile: &ConnectionProfile) -> StudioResult<()> {
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

/// Run a script with no safety policy and no result decoding; the first
/// error ends it.
pub async fn execute_batch_on_client(client: &OracleClient, sql: &str) -> StudioResult<()> {
    let (items_tx, mut items_rx) = mpsc::channel::<Item>(16);
    client
        .requests
        .send(Request::Execute {
            sql: sql.to_string(),
            items: items_tx,
        })
        .await
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?;
    while let Some(item) = items_rx.recv().await {
        if let Item::Error(message) = item {
            return Err(StudioError::QueryExecution(message));
        }
    }
    Ok(())
}

/// Run a script on an open session, streaming results through the shared
/// driver contract (events on the bus, rows on the bounded channel).
/// Applies the profile's safety policy (SELECT-only gate, read-only screen,
/// rollback wrap) phrased for Oracle.
pub async fn execute_on_client(
    client: &OracleClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    let sql = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    let (items_tx, mut items_rx) = mpsc::channel::<Item>(256);
    client
        .requests
        .send(Request::Execute {
            sql,
            items: items_tx,
        })
        .await
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?;

    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    let mut failure: Option<StudioError> = None;
    while let Some(item) = items_rx.recv().await {
        match item {
            Item::ResultSet(columns) => streamer.begin_result_set(columns).await?,
            Item::Row(cells) => {
                if !streamer.push_row(cells).await? {
                    // Consumer gone: drop the receiver so the thread stops
                    // producing, then stop draining.
                    break;
                }
            }
            Item::Affected(n) => streamer.statement_completed(n)?,
            Item::Message(text) => streamer.message(text)?,
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

/// Parameterized, materialized read on an open session (`:1`, `:2`, …
/// placeholders bound by position). Small targeted reads; no safety policy.
pub async fn query_rows_on_client(
    client: &OracleClient,
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
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// Parameterized non-query on an open session; returns affected rows.
pub async fn execute_params_on_client(
    client: &OracleClient,
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
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// Parameterized DML with one text OUT bind — `INSERT … RETURNING ROWID INTO
/// :out_name` — on an open session. Positional `:1`… placeholders carry
/// `params`; `out_name` is the OUT bind's name without the colon. Returns
/// the affected-row count and the OUT value (NULL when nothing was written).
pub async fn execute_returning_text_on_client(
    client: &OracleClient,
    sql: &str,
    params: &[SqlParam],
    out_name: &str,
) -> StudioResult<(u64, Option<String>)> {
    let (tx, rx) = oneshot::channel();
    client
        .requests
        .send(Request::ExecuteReturningText {
            sql: sql.to_string(),
            params: params.to_vec(),
            out_name: out_name.to_string(),
            respond: tx,
        })
        .await
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("oracle connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// The pluggable databases visible to the session (`v$pdbs`), or — when
/// that view is not granted — the one container the session is in.
pub async fn list_databases_on_client(client: &OracleClient) -> StudioResult<Vec<DatabaseSummary>> {
    let pdbs = query_rows_on_client(
        client,
        "SELECT name, open_mode FROM v$pdbs ORDER BY name",
        &[],
    )
    .await;
    let sets = match pdbs {
        Ok(sets) if sets.iter().any(|set| !set.rows.is_empty()) => sets,
        _ => {
            query_rows_on_client(
                client,
                "SELECT SYS_CONTEXT('USERENV', 'CON_NAME') AS name, 'READ WRITE' AS open_mode FROM DUAL",
                &[],
            )
            .await?
        }
    };
    let mut out = Vec::new();
    for set in sets {
        for row in set.rows {
            if let Some(CellValue::String(name)) = row.first() {
                let state = match row.get(1) {
                    Some(CellValue::String(mode)) if mode.starts_with("READ") => "ONLINE",
                    Some(CellValue::String(_)) => "OFFLINE",
                    _ => "ONLINE",
                };
                out.push(DatabaseSummary {
                    name: name.clone(),
                    state: state.to_string(),
                    compatibility_level: None,
                });
            }
        }
    }
    Ok(out)
}

/// `SqlTransport` implementation for Oracle Database, usable with the shared
/// [`ConnectionService`].
#[derive(Debug, Clone, Default)]
pub struct OracleTransport;

#[async_trait::async_trait]
impl SqlTransport for OracleTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        list_databases_on_client(&client).await
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
    params: connect::ConnectParams,
    password: String,
    autocommit: bool,
    mut requests: mpsc::Receiver<Request>,
    ready: oneshot::Sender<Result<Arc<Connection>, StudioError>>,
) {
    init_client_library();
    let mut connector = Connector::new(params.username, password, params.connect_string);
    match params.role {
        SessionRole::Normal => {}
        SessionRole::Sysdba => {
            connector.privilege(Privilege::Sysdba);
        }
        SessionRole::Sysoper => {
            connector.privilege(Privilege::Sysoper);
        }
    }
    let mut conn = match connector.connect() {
        Ok(conn) => conn,
        Err(error) => {
            let _ = ready.send(Err(connect::to_connection_error(error)));
            return;
        }
    };
    conn.set_autocommit(autocommit);
    // Unlimited server-side buffer; a session that cannot see DBMS_OUTPUT
    // simply produces no messages.
    if let Err(error) = conn.execute("BEGIN DBMS_OUTPUT.ENABLE(NULL); END;", &[]) {
        tracing::debug!(%error, "DBMS_OUTPUT.ENABLE failed");
    }
    let conn = Arc::new(conn);
    if ready.send(Ok(Arc::clone(&conn))).is_err() {
        let _ = conn.close();
        return;
    }

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
            Request::ExecuteReturningText {
                sql,
                params,
                out_name,
                respond,
            } => {
                let _ = respond.send(run_execute_returning_text(&conn, &sql, &params, &out_name));
            }
        }
    }
    // Close explicitly: a break handle may still hold the Arc.
    let _ = conn.close();
}

/// Run a script, streaming items. A send failure means the async side hung
/// up (cancellation) — stop quietly; an engine error is sent and ends the
/// batch.
fn run_batch(conn: &Connection, sql: &str, items: &mpsc::Sender<Item>) {
    for statement in split_script(sql) {
        if run_statement(conn, &statement, items).is_err() {
            return;
        }
        if drain_output(conn, items).is_err() {
            return;
        }
    }
}

/// Execute one statement and stream its outcome. `Err(())` means stop the
/// batch: either the consumer went away or an error was already reported.
fn run_statement(
    conn: &Connection,
    statement: &ScriptStatement,
    items: &mpsc::Sender<Item>,
) -> Result<(), ()> {
    let mut stmt = conn
        .statement(&statement.text)
        .fetch_array_size(FETCH_ARRAY_SIZE)
        .build()
        .map_err(|e| report(items, e))?;
    if !stmt.is_query() {
        stmt.execute(&[]).map_err(|e| report(items, e))?;
        // Only DML carries a real row count: OCI reports 1 for any PL/SQL
        // block that ran (DML inside or not) and noise for DDL.
        let affected = if stmt.is_dml() {
            stmt.row_count().unwrap_or(0)
        } else {
            0
        };
        return items
            .blocking_send(Item::Affected(affected))
            .map_err(|_| ());
    }
    let rows = stmt.query(&[]).map_err(|e| report(items, e))?;
    let (columns, kinds) = describe_columns(rows.column_info());
    items
        .blocking_send(Item::ResultSet(columns))
        .map_err(|_| ())?;
    for row in rows {
        let row = row.map_err(|e| report(items, e))?;
        let cells = row
            .sql_values()
            .iter()
            .zip(&kinds)
            .map(|(value, kind)| values::cell_value(value, *kind))
            .collect();
        items.blocking_send(Item::Row(cells)).map_err(|_| ())?;
    }
    Ok(())
}

fn describe_columns(infos: &[oracle::ColumnInfo]) -> (Vec<ResultColumn>, Vec<FetchKind>) {
    let columns = infos.iter().map(values::result_column).collect();
    let kinds = infos.iter().map(|i| fetch_kind(i.oracle_type())).collect();
    (columns, kinds)
}

fn report(items: &mpsc::Sender<Item>, error: oracle::Error) {
    let _ = items.blocking_send(Item::Error(format_error(&error)));
}

/// Oracle's own error text, with a hint for the one fetch the client
/// library cannot do (native `JSON` columns).
fn format_error(error: &oracle::Error) -> String {
    let text = error.to_string();
    if text.contains("unsupported Oracle type JSON") {
        return format!(
            "{text}. Oracle JSON columns cannot be fetched directly through the client library; select JSON_SERIALIZE(column) instead."
        );
    }
    text
}

/// PL/SQL that moves buffered `DBMS_OUTPUT` lines into an OUT bind, up to
/// ~32 KB per call. A line that would overflow the buffer comes back in
/// `:rest` with `:more = 1`, so the caller loops; nothing is lost or
/// reordered (`GET_LINE` is destructive, so a line cannot be put back).
const DRAIN_OUTPUT_SQL: &str = "DECLARE \
  l_line VARCHAR2(32767); l_status INTEGER; l_buf VARCHAR2(32767); l_n PLS_INTEGER := 0; \
BEGIN \
  :more := 0; :rest := NULL; \
  LOOP \
    DBMS_OUTPUT.GET_LINE(l_line, l_status); \
    EXIT WHEN l_status <> 0; \
    IF l_n > 0 AND NVL(LENGTHB(l_buf), 0) + NVL(LENGTHB(l_line), 0) + 1 > 32000 THEN \
      :rest := l_line; :more := 1; EXIT; \
    END IF; \
    l_buf := l_buf || SUBSTRB(NVL(l_line, ' '), 1, 32000) || CHR(10); l_n := l_n + 1; \
  END LOOP; \
  :buf := l_buf; :n := l_n; \
END;";

/// Publish every buffered `DBMS_OUTPUT` line as a message. A session that
/// cannot call `DBMS_OUTPUT` (or has nothing buffered) publishes nothing;
/// only a gone consumer stops the batch.
fn drain_output(conn: &Connection, items: &mpsc::Sender<Item>) -> Result<(), ()> {
    for _ in 0..MAX_OUTPUT_DRAINS {
        let mut stmt = match conn.statement(DRAIN_OUTPUT_SQL).build() {
            Ok(stmt) => stmt,
            Err(error) => {
                tracing::debug!(%error, "DBMS_OUTPUT drain unavailable");
                return Ok(());
            }
        };
        let executed = stmt.execute_named(&[
            ("more", &OracleType::Int64),
            ("rest", &OracleType::Varchar2(32767)),
            ("buf", &OracleType::Varchar2(32767)),
            ("n", &OracleType::Int64),
        ]);
        if let Err(error) = executed {
            tracing::debug!(%error, "DBMS_OUTPUT drain failed");
            return Ok(());
        }
        let count: i64 = stmt.bind_value("n").unwrap_or(0);
        if count > 0 {
            let buf: Option<String> = stmt.bind_value("buf").unwrap_or(None);
            for line in buf.unwrap_or_default().split('\n') {
                if line.is_empty() {
                    continue;
                }
                items
                    .blocking_send(Item::Message(line.trim_end().to_string()))
                    .map_err(|_| ())?;
            }
        }
        let more: i64 = stmt.bind_value("more").unwrap_or(0);
        if more == 0 {
            return Ok(());
        }
        let rest: Option<String> = stmt.bind_value("rest").unwrap_or(None);
        if let Some(rest) = rest {
            items
                .blocking_send(Item::Message(rest.trim_end().to_string()))
                .map_err(|_| ())?;
        }
    }
    Ok(())
}

fn boxed_params(params: &[SqlParam]) -> Vec<Box<dyn ToSql>> {
    params
        .iter()
        .map(|p| -> Box<dyn ToSql> {
            match p {
                SqlParam::Null => Box::new(None::<String>),
                SqlParam::Bool(b) => Box::new(*b),
                SqlParam::I64(n) => Box::new(*n),
                SqlParam::F64(f) => Box::new(*f),
                SqlParam::String(s) => Box::new(s.clone()),
                SqlParam::Bytes(b) => Box::new(b.clone()),
            }
        })
        .collect()
}

fn run_query_rows(
    conn: &Connection,
    sql: &str,
    params: &[SqlParam],
) -> Result<Vec<MaterializedResultSet>, String> {
    let boxed = boxed_params(params);
    let refs: Vec<&dyn ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn
        .statement(sql)
        .fetch_array_size(FETCH_ARRAY_SIZE)
        .build()
        .map_err(|e| format_error(&e))?;
    if !stmt.is_query() {
        stmt.execute(&refs).map_err(|e| format_error(&e))?;
        return Ok(vec![MaterializedResultSet {
            columns: Vec::new(),
            rows: Vec::new(),
        }]);
    }
    let rows = stmt.query(&refs).map_err(|e| format_error(&e))?;
    let (columns, kinds) = describe_columns(rows.column_info());
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| format_error(&e))?;
        out.push(
            row.sql_values()
                .iter()
                .zip(&kinds)
                .map(|(value, kind)| values::cell_value(value, *kind))
                .collect(),
        );
    }
    Ok(vec![MaterializedResultSet { columns, rows: out }])
}

fn run_execute_params(conn: &Connection, sql: &str, params: &[SqlParam]) -> Result<u64, String> {
    let boxed = boxed_params(params);
    let refs: Vec<&dyn ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
    let stmt = conn.execute(sql, &refs).map_err(|e| format_error(&e))?;
    Ok(if stmt.is_dml() {
        stmt.row_count().unwrap_or(0)
    } else {
        0
    })
}

/// Positional IN binds are named `1`, `2`, … (Oracle accepts numeric bind
/// names, which is what `:1` placeholders are) so they can share one
/// `execute_named` call with the text OUT bind.
fn run_execute_returning_text(
    conn: &Connection,
    sql: &str,
    params: &[SqlParam],
    out_name: &str,
) -> Result<(u64, Option<String>), String> {
    let boxed = boxed_params(params);
    let names: Vec<String> = (1..=params.len()).map(|n| n.to_string()).collect();
    let out_type = OracleType::Varchar2(4000);
    let mut named: Vec<(&str, &dyn ToSql)> = names
        .iter()
        .zip(boxed.iter())
        .map(|(name, value)| (name.as_str(), value.as_ref()))
        .collect();
    named.push((out_name, &out_type));
    let mut stmt = conn.statement(sql).build().map_err(|e| format_error(&e))?;
    stmt.execute_named(&named).map_err(|e| format_error(&e))?;
    let affected = if stmt.is_dml() {
        stmt.row_count().unwrap_or(0)
    } else {
        0
    };
    let value: Option<String> = stmt.bind_value(out_name).map_err(|e| format_error(&e))?;
    Ok((affected, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_fetch_error_gets_an_actionable_hint() {
        let error = oracle::Error::new(
            oracle::ErrorKind::InternalError,
            "unsupported Oracle type JSON",
        );
        assert!(format_error(&error).contains("JSON_SERIALIZE"));
        let plain = oracle::Error::new(
            oracle::ErrorKind::OciError,
            "ORA-00942: table or view does not exist",
        );
        assert_eq!(
            format_error(&plain),
            "ORA-00942: table or view does not exist"
        );
    }

    #[test]
    fn params_bind_positionally_by_kind() {
        let boxed = boxed_params(&[
            SqlParam::Null,
            SqlParam::Bool(true),
            SqlParam::I64(1),
            SqlParam::F64(1.5),
            SqlParam::String("x".into()),
            SqlParam::Bytes(vec![1]),
        ]);
        assert_eq!(boxed.len(), 6);
    }

    #[test]
    fn drain_sql_is_one_plsql_block_with_the_four_binds() {
        let parts = split_script(DRAIN_OUTPUT_SQL);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].plsql);
        for bind in [":more", ":rest", ":buf", ":n"] {
            assert!(DRAIN_OUTPUT_SQL.contains(bind), "{bind}");
        }
    }
}
