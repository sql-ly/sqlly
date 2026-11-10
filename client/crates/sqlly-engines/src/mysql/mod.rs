//! # sqlly-mysql
//!
//! MySQL client/server protocol driver mirroring the `sqlly-tds` API surface:
//! [`open_client`] / [`execute_on_client`] for the streaming query path,
//! [`query_rows_on_client`] / [`execute_params_on_client`] for parameterized
//! reads/writes, and [`MySqlTransport`] implementing
//! `crate::driver::SqlTransport`. Serves MySQL, MariaDB, and the
//! MySQL-compatible Aurora/RDS defaults — dispatch keys on
//! `DatabaseType::connection_kind() == ConnectionKind::MySqlWire`.
//!
//! Streaming uses the text protocol (`query_iter`) so multi-statement
//! batches and multiple result sets work like the TDS driver's
//! `simple_query`. TLS follows the profile's [`TlsMode`], fail-closed:
//! encrypted modes negotiate rustls (`Require` accepts any certificate,
//! `VerifyCa` verifies the chain but not the hostname, `VerifyFull` verifies
//! both; the profile's CA file joins the trusted roots and a client
//! certificate/key pair enables mutual TLS) and error out rather than
//! downgrade when the server cannot establish TLS; `Disable` connects
//! plaintext explicitly.

#![forbid(unsafe_code)]

pub mod catalog;

use crate::driver::QueryStreamer;
use crate::sql_text::may_change_database;
use futures::StreamExt;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Column, Row, Value};
use sqlly_core::{
    AuthenticationType, CellValue, ConnectionProfile, DatabaseSummary, EventBus,
    MaterializedResultSet, ResultColumn, RowSender, SqlParam, StudioError, StudioResult, TlsMode,
};
use sqlly_lang::sqlmodel::Dialect;

pub use crate::driver::{ConnectionService, SqlTransport};

/// A live MySQL connection.
pub struct MySqlClient {
    conn: tokio::sync::Mutex<mysql_async::Conn>,
    /// The last [`execute_on_client`] run left the session somewhere other
    /// than the profile's database, or somewhere it could not confirm. Set on
    /// failed runs too, where there is no [`ExecuteOutcome`] to carry it.
    left_profile_database: std::sync::atomic::AtomicBool,
}

impl MySqlClient {
    /// Whether the last run left this session off the profile's database (or
    /// unconfirmed). A caller keeping the client after a *failed* run — a
    /// pinned manual-transaction session — must check this, since the error
    /// return carries no [`ExecuteOutcome`].
    pub fn left_profile_database(&self) -> bool {
        self.left_profile_database
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Build connection options from a profile. Public for tests.
pub fn opts_from_profile(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
) -> StudioResult<mysql_async::Opts> {
    let (username, password) = match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => (username.clone(), secret_ref.resolve()?),
        AuthenticationType::Integrated => {
            return Err(StudioError::NotImplemented(
                "Integrated authentication is not supported for MySQL connections",
            ));
        }
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            return Err(StudioError::InvalidProfile(
                "Microsoft Entra authentication applies to SQL Server, not MySQL".into(),
            ));
        }
        AuthenticationType::AwsRdsIam { username, .. }
        | AuthenticationType::GcpCloudSqlIam { username } => {
            let token = profile.runtime_cloud_iam_token.clone().ok_or_else(|| {
                StudioError::AuthenticationMessage(sqlly_core::DisplayMessage::new(
                    "services.cloud_auth.iam_token_missing",
                    "Authentication failed: cloud IAM token missing; connect from SQLLY (or pass --access-token / SQLLY_CLOUD_IAM_TOKEN for the CLI)",
                ))
            })?;
            (username.clone(), token)
        }
        AuthenticationType::AuthToken { .. } => {
            return Err(StudioError::InvalidProfile(
                "Auth-token sign-in applies to libSQL / Turso, not MySQL".into(),
            ));
        }
    };
    let database = profile
        .database
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let builder = mysql_async::OptsBuilder::default()
        .ip_or_hostname(host.trim().to_string())
        .tcp_port(port)
        // The driver sends no keepalives by default; see `tcp_keepalive`.
        .tcp_keepalive(Some(crate::tcp_keepalive::IDLE))
        .user(Some(username))
        .pass(Some(password))
        .db_name(database)
        .ssl_opts(ssl_opts_for_profile(profile)?.map(|opts| {
            // Through an SSH tunnel or proxy the driver dials the tunnel's
            // local end; the certificate names the server.
            opts.with_danger_tls_hostname_override(
                crate::ssh::server_name_behind_tunnel(profile, host).map(str::to_string),
            )
        }))
        .enable_cleartext_plugin(cleartext_password_allowed(profile))
        // Statement caching is off for correctness: re-executing a cached
        // prepared statement with a DIFFERENT parameter type corrupts values
        // on MySQL 8.4 (the server coerces through the previous execution's
        // types — e.g. a Bytes param after a Double comes back as 0.0).
        // Verified live against mysql:8; MariaDB is unaffected. Re-preparing
        // per call costs one round trip on the parameterized paths only.
        .stmt_cache_size(0);
    Ok(mysql_async::Opts::from(builder))
}

/// Whether the server may ask for the password in cleartext
/// (`mysql_clear_password`). Cloud IAM tokens are always sent that way (and
/// force TLS). A SQL login gets it only over TLS with a verified server
/// certificate, which is what PAM and LDAP accounts need; over plaintext or
/// unverified TLS a server (or anyone in between) could ask for the
/// password, so the request is refused.
fn cleartext_password_allowed(profile: &ConnectionProfile) -> bool {
    profile.authentication.is_cloud_iam()
        || matches!(
            crate::pg::tls_mode_for_profile(profile),
            TlsMode::VerifyCa | TlsMode::VerifyFull
        )
}

/// Map the profile's [`TlsMode`] onto `mysql_async` TLS options.
/// `None` is explicit plaintext; `Some` requires TLS (the driver fails the
/// handshake rather than downgrading when the server cannot negotiate it).
/// `Require` keeps the link encrypted but skips certificate checks — the
/// same trade the TDS driver makes via `trust_cert()`; `VerifyCa` verifies
/// the chain but not the hostname; `VerifyFull` verifies both. The profile's
/// CA file joins the trusted roots and a client certificate/key pair (both
/// PEM, validated up front) enables mutual TLS.
fn ssl_opts_for_profile(profile: &ConnectionProfile) -> StudioResult<Option<mysql_async::SslOpts>> {
    let mode = crate::pg::tls_mode_for_profile(profile);
    if mode == TlsMode::Disable {
        return Ok(None);
    }
    let mut opts = match mode {
        TlsMode::Require => mysql_async::SslOpts::default()
            .with_danger_accept_invalid_certs(true)
            .with_danger_skip_domain_validation(true),
        TlsMode::VerifyCa => {
            mysql_async::SslOpts::default().with_danger_skip_domain_validation(true)
        }
        TlsMode::VerifyFull | TlsMode::Legacy | TlsMode::Disable => mysql_async::SslOpts::default(),
    };
    if let Some(ca_path) = profile.tls_ca_file.as_deref() {
        // Validate eagerly for the same fail-fast errors as the PG driver
        // (unreadable file, bundle with zero certificates).
        crate::pg::load_ca_certs(ca_path)?;
        opts = opts.with_root_certs(vec![ca_path.to_path_buf().into()]);
    }
    if let Some((cert_path, key_path)) = crate::pg::client_identity_paths(profile)? {
        // Same up-front validation as PG, including the unencrypted-PEM-only
        // rule for the private key; mysql_async re-reads the files at
        // handshake time.
        crate::pg::load_client_identity(cert_path, key_path)?;
        opts = opts.with_client_identity(Some(mysql_async::ClientIdentity::new(
            cert_path.to_path_buf().into(),
            key_path.to_path_buf().into(),
        )));
    }
    Ok(Some(opts))
}

/// Open a client. The handshake is bounded by the profile timeout. Encrypted
/// profiles fail closed: a server that cannot negotiate TLS (or presents a
/// certificate that fails validation without `trust_server_certificate`)
/// produces an error instead of a silent plaintext downgrade.
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<MySqlClient> {
    open_with(profile, false).await
}

/// Open a client with the `CLIENT_FOUND_ROWS` capability: UPDATE reports the
/// number of rows *matched* rather than *changed*. The row editor's guarded
/// writes need matched semantics — a guarded UPDATE whose WHERE clause found
/// the row must count 1 even when the new values happen to equal the current
/// ones, otherwise it would be misreported as a concurrency conflict. Keep
/// these clients on their own pool lane; query lanes expect default
/// changed-rows counts.
pub async fn open_client_found_rows(profile: &ConnectionProfile) -> StudioResult<MySqlClient> {
    open_with(profile, true).await
}

async fn open_with(profile: &ConnectionProfile, found_rows: bool) -> StudioResult<MySqlClient> {
    let (host, port) = crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    let mut opts = opts_from_profile(profile, &host, port)?;
    if found_rows {
        opts = mysql_async::Opts::from(
            mysql_async::OptsBuilder::from_opts(opts).client_found_rows(true),
        );
    }
    let conn = tokio::time::timeout(profile.timeout(), mysql_async::Conn::new(opts))
        .await
        .map_err(|_| {
            StudioError::Connection(format!(
                "timed out connecting to {} after {:?}",
                profile.server,
                profile.timeout()
            ))
        })?
        .map_err(to_connection_error)?;
    let client = MySqlClient {
        conn: tokio::sync::Mutex::new(conn),
        left_profile_database: std::sync::atomic::AtomicBool::new(false),
    };
    apply_startup_sql(&client, profile).await?;
    Ok(client)
}

async fn apply_startup_sql(client: &MySqlClient, profile: &ConnectionProfile) -> StudioResult<()> {
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

/// Server-side cancellation handle for a [`MySqlClient`]. Captured while the
/// client is idle; [`MySqlCancelHandle::cancel`] opens a short side connection
/// and issues `KILL QUERY <id>`, which stops the connection's in-flight
/// statement without terminating the session. MySQL allows killing queries on
/// connections owned by the same user, so no extra privilege is required.
pub struct MySqlCancelHandle {
    opts: mysql_async::Opts,
    connection_id: u32,
}

impl MySqlCancelHandle {
    /// Ask the server to stop the connection's running statement. Best-effort:
    /// a failure means the statement runs to completion server-side.
    pub async fn cancel(self) -> StudioResult<()> {
        let mut side = mysql_async::Conn::new(self.opts)
            .await
            .map_err(to_connection_error)?;
        let result = side
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .await
            .map_err(|e| StudioError::QueryExecution(format!("cancel request failed: {e}")));
        let _ = side.disconnect().await;
        result
    }
}

/// Obtain a cancellation handle for the client's connection. Must be called
/// while the client is idle (it briefly locks the connection to read its
/// server-assigned id).
pub async fn cancel_handle(
    client: &MySqlClient,
    profile: &ConnectionProfile,
) -> StudioResult<MySqlCancelHandle> {
    let connection_id = client.conn.lock().await.id();
    let (host, port) = crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    Ok(MySqlCancelHandle {
        opts: opts_from_profile(profile, &host, port)?,
        connection_id,
    })
}

/// Result of a completed [`execute_on_client`] run. Mirrors the pool-safety
/// half of `crate::tds::ExecuteOutcome`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecuteOutcome {
    /// A `USE` moved the session off the profile's database (or left it with
    /// none selected, when the profile names one). The warm client
    /// no longer matches its pool key, so callers MUST discard it rather than
    /// return it to the pool.
    pub database_changed: bool,
    /// The post-run `DATABASE()` confirmation could not be read (the results
    /// were already delivered, so this is not a hard failure). The client's
    /// live database is unknown, so it MUST be discarded rather than pooled.
    pub database_confirmation_failed: bool,
}

impl ExecuteOutcome {
    /// Whether the warm client must be dropped instead of returned to the pool.
    pub fn should_discard_client(&self) -> bool {
        self.database_changed || self.database_confirmation_failed
    }
}

/// Run a statement batch on an open client via the text protocol, streaming
/// every result set through the shared driver contract. Applies the
/// profile's safety policy with MySQL transaction syntax.
pub async fn execute_on_client(
    client: &MySqlClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<ExecuteOutcome> {
    let sql = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    // Decided before the text is handed to the driver: the confirmation after
    // the run needs it, and `query_iter` consumes the statement.
    let may_use = may_change_database(&sql, Dialect::MySql);
    client
        .left_profile_database
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut conn = client.conn.lock().await;
    let outcome = run_batch(&mut conn, client, profile, sql, may_use, events, rows_tx).await;
    if outcome.is_err() && (profile.auto_wrap_rollback || profile.enforce_read_only) {
        // A statement error stops a multi-statement batch before the wrap's
        // closing ROLLBACK, leaving its transaction open on this pooled
        // connection — where a later implicit commit (DDL, START
        // TRANSACTION) would make the "rolled back" work stick. Close it
        // now; best effort, the original error is what the user sees.
        let _ = conn.query_drop("ROLLBACK").await;
    }
    outcome
}

/// Stream every result set of an already-policied batch, then confirm the
/// session's database when the batch may have run a `USE`.
async fn run_batch(
    conn: &mut mysql_async::Conn,
    client: &MySqlClient,
    profile: &ConnectionProfile,
    sql: String,
    may_use: bool,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<ExecuteOutcome> {
    let mut result = conn.query_iter(sql).await.map_err(to_query_error)?;

    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    // A failure mid-batch must not skip the confirmation below: in
    // `USE other; SELECT * FROM missing` the USE has already moved the session
    // by the time the SELECT fails.
    let streamed = stream_result_sets(&mut result, &mut streamer).await;

    // MySQL's `USE db` silently retargets the session, and the profile (and the
    // warm-pool key built from it) still names the old database. SQL Server
    // confirms the same way after a `USE`; without this the app keeps showing
    // the profile's database while every later statement runs somewhere else,
    // and the connection returns to the pool under a stale key.
    // The result stream borrows the connection; the confirmation needs it back.
    drop(result);
    let mut outcome = ExecuteOutcome::default();
    if may_use {
        match current_database(conn).await {
            Ok(Some(current)) => {
                // Exact comparison: MySQL database names are case-sensitive
                // wherever `lower_case_table_names = 0` (the Linux default), so
                // `Sales` and `sales` can be two databases. On servers that fold
                // case this can only over-report a change, which costs one
                // reconnect — never a query run in the wrong database.
                let prior = profile.database.as_deref().unwrap_or("");
                if current != prior {
                    outcome.database_changed = true;
                    streamer.database_changed(current)?;
                }
            }
            // No database selected any more (`USE scratch; DROP DATABASE
            // scratch`). If the profile names one, the session has left it:
            // there is no name to announce, but the client must not go back to
            // the pool under that key.
            Ok(None) => outcome.database_changed = profile.database.is_some(),
            Err(_) => {
                // The rows already streamed, so a failed confirmation must not
                // turn a delivered run into an error. Say so and have the caller
                // discard the client rather than re-pool it under a key whose
                // database we can no longer vouch for.
                outcome.database_confirmation_failed = true;
                streamer.message(
                    "Could not confirm the current database after execution; \
                     reconnecting before the next query.",
                )?;
            }
        }
    }

    client.left_profile_database.store(
        outcome.should_discard_client(),
        std::sync::atomic::Ordering::Relaxed,
    );
    streamed?;
    streamer.finish().await?;
    Ok(outcome)
}

/// Stream every result set of a text-protocol batch through `streamer`.
/// Stops early (with `Ok`) when the consumer stops accepting rows.
async fn stream_result_sets(
    result: &mut mysql_async::QueryResult<'_, '_, mysql_async::TextProtocol>,
    streamer: &mut QueryStreamer<'_>,
) -> StudioResult<()> {
    'sets: loop {
        let columns: Vec<ResultColumn> = result
            .columns()
            .map(|cols| cols.iter().map(column_to_result_column).collect())
            .unwrap_or_default();
        let affected = result.affected_rows();
        let Some(mut rows) = result.stream::<Row>().await.map_err(to_query_error)? else {
            break 'sets;
        };
        if columns.is_empty() {
            // A statement with no result set (DML/DDL): count only.
            streamer.statement_completed(affected)?;
            // Drain the (empty) stream so the next set becomes current.
            while rows
                .next()
                .await
                .transpose()
                .map_err(to_query_error)?
                .is_some()
            {}
            continue 'sets;
        }
        streamer.begin_result_set(columns.clone()).await?;
        loop {
            // Race the next row against the pending batch's flush deadline so
            // a slow producer still surfaces buffered rows at the shared
            // cadence instead of holding them until the next row arrives.
            let row = match streamer.pending_flush_deadline() {
                Some(deadline) => tokio::select! {
                    biased;
                    row = rows.next() => row,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        streamer.flush_now().await;
                        continue;
                    }
                },
                None => rows.next().await,
            };
            let Some(row) = row.transpose().map_err(to_query_error)? else {
                break;
            };
            let cells = row_to_cells(&row);
            if !streamer.push_row(cells).await? {
                break 'sets;
            }
        }
    }
    Ok(())
}

/// The session's live database, or `None` when the connection has none
/// selected. `DATABASE()` is the MySQL counterpart of SQL Server's `DB_NAME()`.
async fn current_database(conn: &mut mysql_async::Conn) -> StudioResult<Option<String>> {
    let value: Option<Option<String>> = conn
        .query_first("SELECT DATABASE()")
        .await
        .map_err(to_query_error)?;
    Ok(value.flatten().filter(|name| !name.is_empty()))
}

/// Parameterized, materialized read (`?` placeholders). Mirrors
/// `crate::tds::query_rows_on_client`; no safety policy applied.
pub async fn query_rows_on_client(
    client: &MySqlClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<Vec<MaterializedResultSet>> {
    let mut conn = client.conn.lock().await;
    let mut result = conn
        .exec_iter(sql, params_to_mysql(params))
        .await
        .map_err(to_query_error)?;
    let columns: Vec<ResultColumn> = result
        .columns()
        .map(|cols| cols.iter().map(column_to_result_column).collect())
        .unwrap_or_default();
    let rows: Vec<Row> = result.collect().await.map_err(to_query_error)?;
    Ok(vec![MaterializedResultSet {
        columns,
        rows: rows.iter().map(row_to_cells).collect(),
    }])
}

/// Origin metadata for one column of a prepared — never executed — statement.
/// The MySQL protocol's column definitions carry the source schema, original
/// (pre-alias) table, and original column name; all three are empty for
/// expression columns. This is the MySQL equivalent of
/// `sp_describe_first_result_set` provenance: the server itself reports where
/// each projected column comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlColumnOrigin {
    pub name: String,
    pub schema: String,
    pub base_table: String,
    pub origin_column: String,
}

/// Prepare `sql` on the client and return each result column's origin. The
/// statement is parsed server-side but never executed, and is closed before
/// returning.
pub async fn describe_statement_on_client(
    client: &MySqlClient,
    sql: &str,
) -> StudioResult<Vec<MySqlColumnOrigin>> {
    let mut conn = client.conn.lock().await;
    let statement = conn.prep(sql).await.map_err(to_query_error)?;
    let origins = statement
        .columns()
        .iter()
        .map(|column| MySqlColumnOrigin {
            name: column.name_str().to_string(),
            schema: column.schema_str().to_string(),
            base_table: column.org_table_str().to_string(),
            origin_column: column.org_name_str().to_string(),
        })
        .collect();
    conn.close(statement).await.map_err(to_query_error)?;
    Ok(origins)
}

/// Run a raw statement with no parameters and no result decoding. Used for
/// transaction control (`START TRANSACTION`/`COMMIT`/`ROLLBACK`) on the
/// row-editing path; no safety policy is applied.
pub async fn execute_batch_on_client(client: &MySqlClient, sql: &str) -> StudioResult<()> {
    let mut conn = client.conn.lock().await;
    conn.query_drop(sql).await.map_err(to_query_error)
}

/// Parameterized non-query; returns affected rows. No safety policy.
pub async fn execute_params_on_client(
    client: &MySqlClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<u64> {
    let mut conn = client.conn.lock().await;
    let result = conn
        .exec_iter(sql, params_to_mysql(params))
        .await
        .map_err(to_query_error)?;
    let affected = result.affected_rows();
    result.drop_result().await.map_err(to_query_error)?;
    Ok(affected)
}

fn params_to_mysql(params: &[SqlParam]) -> mysql_async::Params {
    if params.is_empty() {
        return mysql_async::Params::Empty;
    }
    mysql_async::Params::Positional(
        params
            .iter()
            .map(|p| match p {
                SqlParam::Null => Value::NULL,
                SqlParam::Bool(b) => Value::Int(i64::from(*b)),
                SqlParam::I64(n) => Value::Int(*n),
                SqlParam::F64(f) => Value::Double(*f),
                SqlParam::String(s) => Value::Bytes(s.clone().into_bytes()),
                SqlParam::Bytes(b) => Value::Bytes(b.clone()),
            })
            .collect(),
    )
}

fn column_to_result_column(column: &Column) -> ResultColumn {
    ResultColumn {
        name: column.name_str().to_string(),
        data_type: column_type_to_sql_name(column).to_string(),
        nullable: !column.flags().contains(ColumnFlags::NOT_NULL_FLAG),
    }
}

/// The SQL spelling of a wire column type, so the grid and the exports see
/// `int`/`bigint`/`decimal` rather than the protocol's `LONG`/`LONGLONG`/
/// `NEWDECIMAL`. Unsigned integers keep the qualifier in the name
/// (`int unsigned`) the way `SHOW COLUMNS` prints them. Rendered values
/// still travel as text on the text protocol; the declared type is what
/// tells a consumer a `"42"` is a number.
fn column_type_to_sql_name(column: &Column) -> &'static str {
    use ColumnType::*;
    let unsigned = column.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = is_binary_column(column);
    match column.column_type() {
        MYSQL_TYPE_TINY if unsigned => "tinyint unsigned",
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT if unsigned => "smallint unsigned",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 if unsigned => "mediumint unsigned",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG if unsigned => "int unsigned",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG if unsigned => "bigint unsigned",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_VECTOR => "vector",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        MYSQL_TYPE_STRING => {
            if binary {
                "binary"
            } else {
                "char"
            }
        }
        MYSQL_TYPE_TYPED_ARRAY | MYSQL_TYPE_UNKNOWN => "unknown",
    }
}

fn row_to_cells(row: &Row) -> Vec<CellValue> {
    let columns = row.columns_ref();
    (0..columns.len())
        .map(|i| {
            row.as_ref(i)
                .map(|value| value_to_cell(value, &columns[i]))
                .unwrap_or(CellValue::Null)
        })
        .collect()
}

/// Whether a column's values are raw binary payloads rather than rendered
/// text. `BLOB` and `VAR_STRING` double as `TEXT` and `VARCHAR`, so those types
/// also need the binary charset (63) and `BINARY_FLAG`. `BIT` is always raw
/// bytes (`b'1'` is `0x01`) but carries only `UNSIGNED`, never `BINARY_FLAG`.
/// Temporal and numeric types never carry binary payloads no matter what
/// `BINARY_FLAG` says about them.
fn is_binary_column(column: &Column) -> bool {
    match column.column_type() {
        ColumnType::MYSQL_TYPE_BIT => true,
        ColumnType::MYSQL_TYPE_BLOB
        | ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_GEOMETRY => {
            column.character_set() == 63 && column.flags().contains(ColumnFlags::BINARY_FLAG)
        }
        _ => false,
    }
}

/// Map a text/binary protocol value to the canonical `CellValue` shape:
/// native for ints/floats, strings for text and temporal values, bytes only
/// for genuinely binary columns.
fn value_to_cell(value: &Value, column: &Column) -> CellValue {
    match value {
        Value::NULL => CellValue::Null,
        Value::Int(n) => CellValue::I64(*n),
        Value::UInt(n) => i64::try_from(*n)
            .map(CellValue::I64)
            .unwrap_or_else(|_| CellValue::String(n.to_string())),
        Value::Float(f) => CellValue::F64((*f).into()),
        Value::Double(f) => CellValue::F64(*f),
        Value::Bytes(bytes) => {
            // The text protocol hands every value over as bytes, so the column
            // metadata is all we have to tell a real blob from a rendered
            // value. `BINARY_FLAG` alone does not: MySQL sets it on anything
            // that is not a character column, which includes DATETIME/DATE/
            // TIME/TIMESTAMP and every computed expression (`COUNT(*)`, `SUM`,
            // a literal). Only a genuinely binary *type* — and charset 63, which
            // separates BLOB from TEXT and VARBINARY from VARCHAR — is bytes.
            if is_binary_column(column) {
                CellValue::Bytes(bytes.clone())
            } else {
                match std::str::from_utf8(bytes) {
                    Ok(text) => CellValue::String(text.to_string()),
                    // The server labeled the column with a text charset but
                    // the payload is not valid UTF-8 (e.g. binary parameter
                    // echoes, mis-declared columns). Preserve the bytes
                    // verbatim rather than corrupting them with lossy
                    // replacement characters.
                    Err(_) => CellValue::Bytes(bytes.clone()),
                }
            }
        }
        Value::Date(year, month, day, hour, minute, second, micros) => {
            if *hour == 0 && *minute == 0 && *second == 0 && *micros == 0 {
                CellValue::String(format!("{year:04}-{month:02}-{day:02}"))
            } else if *micros == 0 {
                CellValue::String(format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
                ))
            } else {
                CellValue::String(format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"
                ))
            }
        }
        Value::Time(negative, days, hours, minutes, seconds, micros) => {
            let sign = if *negative { "-" } else { "" };
            let total_hours = u32::from(*hours) + days * 24;
            if *micros == 0 {
                CellValue::String(format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}"))
            } else {
                CellValue::String(format!(
                    "{sign}{total_hours:02}:{minutes:02}:{seconds:02}.{micros:06}"
                ))
            }
        }
    }
}

fn to_connection_error(error: mysql_async::Error) -> StudioError {
    if matches!(
        error,
        mysql_async::Error::Driver(mysql_async::DriverError::CleartextPluginDisabled)
    ) {
        return StudioError::AuthenticationMessage(sqlly_core::DisplayMessage::new(
            "services.mysql.cleartext_guidance",
            "This MySQL account signs in with a cleartext password (PAM or LDAP authentication). SQLLY only sends it over an encrypted connection with a verified server certificate: set TLS to Verify CA or Verify full in the connection editor.",
        ));
    }
    let text = error.to_string();
    if text.contains("Access denied") {
        StudioError::Authentication(text)
    } else {
        crate::connect_hint::connection_with_local_network_hint(text)
    }
}

fn to_query_error(error: mysql_async::Error) -> StudioError {
    StudioError::QueryExecution(error.to_string())
}

/// `SqlTransport` implementation for the MySQL wire protocol.
#[derive(Debug, Clone, Default)]
pub struct MySqlTransport;

#[async_trait::async_trait]
impl SqlTransport for MySqlTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        let mut conn = client.conn.lock().await;
        let names: Vec<String> = conn.query("SHOW DATABASES").await.map_err(to_query_error)?;
        Ok(names
            .into_iter()
            .map(|name| DatabaseSummary {
                name,
                state: "ONLINE".to_string(),
                compatibility_level: None,
            })
            .collect())
    }

    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()> {
        let client = open_client(profile).await?;
        execute_on_client(&client, profile, sql, events, rows_tx)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn mysql_profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "mysql",
            "localhost",
            "root",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        p.port = 3306;
        p.database = Some("appdb".into());
        p.database_type = sqlly_core::DatabaseType::MySql;
        p
    }

    fn opts_for(profile: &ConnectionProfile) -> StudioResult<mysql_async::Opts> {
        opts_from_profile(profile, profile.server.trim(), profile.port)
    }

    fn column_stub() -> Column {
        Column::new(mysql_async::consts::ColumnType::MYSQL_TYPE_VAR_STRING)
            .with_name(b"c")
            .with_character_set(224) // utf8mb4
    }

    fn binary_column_stub() -> Column {
        Column::new(mysql_async::consts::ColumnType::MYSQL_TYPE_BLOB)
            .with_name(b"b")
            .with_character_set(63)
            .with_flags(ColumnFlags::BINARY_FLAG)
    }

    #[test]
    fn json_null_stays_distinct_from_sql_null() {
        // `JSON_EXTRACT('{"a":null}', '$.a')` is the JSON value `null`, which
        // `IS NULL` does not match; only a protocol NULL is a NULL cell.
        let json_col = Column::new(mysql_async::consts::ColumnType::MYSQL_TYPE_JSON)
            .with_name(b"j")
            .with_character_set(224)
            .with_flags(ColumnFlags::BINARY_FLAG);
        assert_eq!(
            value_to_cell(&Value::Bytes(b"null".to_vec()), &json_col),
            CellValue::String("null".into())
        );
        assert_eq!(value_to_cell(&Value::NULL, &json_col), CellValue::Null);
    }

    #[test]
    fn wire_column_types_map_to_sql_names() {
        use mysql_async::consts::ColumnType::*;
        let named = |ty, flags: ColumnFlags, charset| {
            column_to_result_column(
                &Column::new(ty)
                    .with_name(b"c")
                    .with_character_set(charset)
                    .with_flags(flags),
            )
            .data_type
        };
        let none = ColumnFlags::empty();
        assert_eq!(named(MYSQL_TYPE_LONG, none, 63), "int");
        assert_eq!(
            named(MYSQL_TYPE_LONG, ColumnFlags::UNSIGNED_FLAG, 63),
            "int unsigned"
        );
        assert_eq!(named(MYSQL_TYPE_LONGLONG, none, 63), "bigint");
        assert_eq!(named(MYSQL_TYPE_TINY, none, 63), "tinyint");
        assert_eq!(named(MYSQL_TYPE_SHORT, none, 63), "smallint");
        assert_eq!(named(MYSQL_TYPE_INT24, none, 63), "mediumint");
        assert_eq!(named(MYSQL_TYPE_NEWDECIMAL, none, 63), "decimal");
        assert_eq!(named(MYSQL_TYPE_DOUBLE, none, 63), "double");
        assert_eq!(named(MYSQL_TYPE_FLOAT, none, 63), "float");
        assert_eq!(named(MYSQL_TYPE_DATETIME, none, 63), "datetime");
        assert_eq!(named(MYSQL_TYPE_VAR_STRING, none, 224), "varchar");
        assert_eq!(
            named(MYSQL_TYPE_VAR_STRING, ColumnFlags::BINARY_FLAG, 63),
            "varbinary"
        );
        assert_eq!(named(MYSQL_TYPE_BLOB, none, 224), "text");
        assert_eq!(named(MYSQL_TYPE_BLOB, ColumnFlags::BINARY_FLAG, 63), "blob");
        assert_eq!(named(MYSQL_TYPE_JSON, none, 63), "json");
    }

    #[test]
    fn cloud_iam_enables_cleartext_plugin_and_ssl() {
        let mut p = mysql_profile();
        p.encrypt = false;
        p.authentication = AuthenticationType::AwsRdsIam {
            username: "jane".into(),
            region: "us-west-2".into(),
            profile: None,
        };
        p.runtime_cloud_iam_token = Some("tok".into());
        let opts = opts_for(&p).unwrap();
        assert!(opts.enable_cleartext_plugin());
        assert!(opts.ssl_opts().is_some());
    }

    #[test]
    fn sql_login_over_verified_tls_allows_cleartext_plugin() {
        // PAM / LDAP accounts need mysql_clear_password; the password is
        // only offered once the server's certificate has been verified.
        let mut p = mysql_profile();
        for mode in [TlsMode::VerifyFull, TlsMode::VerifyCa] {
            p.tls_mode = mode;
            assert!(opts_for(&p).unwrap().enable_cleartext_plugin(), "{mode:?}");
        }
    }

    #[test]
    fn sql_login_without_verified_tls_keeps_cleartext_plugin_off() {
        let mut p = mysql_profile();
        for mode in [TlsMode::Disable, TlsMode::Require] {
            p.tls_mode = mode;
            assert!(!opts_for(&p).unwrap().enable_cleartext_plugin(), "{mode:?}");
        }
    }

    #[test]
    fn refused_cleartext_request_explains_how_to_allow_it() {
        let error = to_connection_error(mysql_async::DriverError::CleartextPluginDisabled.into());
        let StudioError::AuthenticationMessage(message) = error else {
            panic!("expected an authentication error, got {error:?}");
        };
        assert!(message.fallback().contains("Verify CA"), "{message}");
        assert!(message.fallback().contains("PAM"), "{message}");
        assert_eq!(message.key(), Some("services.mysql.cleartext_guidance"));
    }

    #[test]
    fn ssl_opts_follow_profile_encryption_posture() {
        let mut p = mysql_profile();
        p.encrypt = false;
        assert!(opts_for(&p).unwrap().ssl_opts().is_none());

        p.encrypt = true;
        p.trust_server_certificate = false;
        let strict = opts_for(&p).unwrap();
        let ssl = strict.ssl_opts().expect("encrypt=true must set ssl_opts");
        assert!(!ssl.accept_invalid_certs());
        assert!(!ssl.skip_domain_validation());

        p.trust_server_certificate = true;
        let trusting = opts_for(&p).unwrap();
        let ssl = trusting.ssl_opts().expect("encrypt=true must set ssl_opts");
        assert!(ssl.accept_invalid_certs());
        assert!(ssl.skip_domain_validation());
    }

    #[test]
    fn require_sets_both_danger_flags() {
        let mut p = mysql_profile();
        p.tls_mode = sqlly_core::TlsMode::Require;
        let ssl = ssl_opts_for_profile(&p)
            .unwrap()
            .expect("Require must set ssl_opts");
        assert!(ssl.accept_invalid_certs());
        assert!(ssl.skip_domain_validation());
    }

    #[test]
    fn verify_ca_skips_domain_only() {
        let mut p = mysql_profile();
        p.tls_mode = sqlly_core::TlsMode::VerifyCa;
        let ssl = ssl_opts_for_profile(&p)
            .unwrap()
            .expect("VerifyCa must set ssl_opts");
        assert!(!ssl.accept_invalid_certs());
        assert!(ssl.skip_domain_validation());

        p.tls_mode = sqlly_core::TlsMode::VerifyFull;
        let ssl = ssl_opts_for_profile(&p)
            .unwrap()
            .expect("VerifyFull must set ssl_opts");
        assert!(!ssl.accept_invalid_certs());
        assert!(!ssl.skip_domain_validation());

        p.tls_mode = sqlly_core::TlsMode::Disable;
        assert!(ssl_opts_for_profile(&p).unwrap().is_none());
    }

    #[test]
    fn tls_cert_paths_flow_into_ssl_opts() {
        // The validation shared with the PG driver applies here too: a CA
        // file with zero certificates and a cert-without-key pairing both
        // fail before any socket is opened.
        let dir = tempfile::tempdir().unwrap();
        let mut p = mysql_profile();
        p.tls_mode = sqlly_core::TlsMode::VerifyFull;
        let empty = dir.path().join("empty-ca.pem");
        std::fs::write(&empty, "not a cert\n").unwrap();
        p.tls_ca_file = Some(empty);
        let err = ssl_opts_for_profile(&p).unwrap_err();
        assert!(
            err.to_string().contains("no certificates"),
            "unexpected error: {err}"
        );

        p.tls_ca_file = None;
        p.tls_client_cert_file = Some(dir.path().join("client.pem"));
        let err = ssl_opts_for_profile(&p).unwrap_err();
        assert!(
            err.to_string().contains("without its private key"),
            "unexpected error: {err}"
        );
    }

    /// A server (or a proxy in front of one) that accepts the TCP connection
    /// but never sends the handshake must fail with a timeout, not spin.
    #[tokio::test]
    async fn stalled_handshake_times_out_instead_of_hanging() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _hold = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(socket);
        });
        let mut p = mysql_profile();
        p.server = "127.0.0.1".into();
        p.port = port;
        p.timeout_seconds = 1;

        let started = std::time::Instant::now();
        let error = open_client(&p)
            .await
            .err()
            .expect("a silent server must not connect");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    /// Live TLS test: set SQLLY_MYSQL_TLS=true plus SQLLY_MYSQL_SERVER /
    /// SQLLY_MYSQL_USER / SQLLY_MYSQL_PASSWORD (and optionally
    /// SQLLY_MYSQL_PORT / SQLLY_MYSQL_DATABASE) against a TLS-enabled MySQL
    /// (a stock mysql:8 container works — TLS is on with a self-signed cert).
    #[tokio::test]
    #[ignore = "requires a live TLS-enabled MySQL server (SQLLY_MYSQL_* env vars + SQLLY_MYSQL_TLS=true)"]
    async fn live_tls_connects_encrypted_and_cancels() {
        if std::env::var("SQLLY_MYSQL_TLS").as_deref() != Ok("true") {
            return;
        }
        let mut p = live_profile();
        p.encrypt = true;
        p.trust_server_certificate = true;

        let client = open_client(&p).await.unwrap();
        let events = EventBus::new(256);
        let _sub = events.subscribe();
        execute_on_client(&client, &p, "SELECT 1;", &events, None)
            .await
            .unwrap();

        // The cancel side connection inherits the same TLS posture.
        let handle = cancel_handle(&client, &p).await.unwrap();
        handle.cancel().await.unwrap();
    }

    /// Live TLS negative test: a self-signed server must be REJECTED when
    /// `trust_server_certificate` is off — encrypted profiles fail closed.
    #[tokio::test]
    #[ignore = "requires a live TLS-enabled MySQL server with a self-signed cert (SQLLY_MYSQL_* env vars + SQLLY_MYSQL_TLS=true)"]
    async fn live_tls_rejects_untrusted_certificate_fail_closed() {
        if std::env::var("SQLLY_MYSQL_TLS").as_deref() != Ok("true") {
            return;
        }
        let mut p = live_profile();
        p.encrypt = true;
        p.trust_server_certificate = false;

        assert!(
            open_client(&p).await.is_err(),
            "encrypted profile must reject a certificate that fails validation"
        );
    }

    fn live_profile() -> ConnectionProfile {
        let server = std::env::var("SQLLY_MYSQL_SERVER").expect("SQLLY_MYSQL_SERVER");
        let user = std::env::var("SQLLY_MYSQL_USER").expect("SQLLY_MYSQL_USER");
        let mut p = ConnectionProfile::sql_login(
            "mysql-live",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_MYSQL_PASSWORD".into(),
            },
        );
        p.port = std::env::var("SQLLY_MYSQL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3306);
        p.database = std::env::var("SQLLY_MYSQL_DATABASE").ok();
        p.database_type = sqlly_core::DatabaseType::MySql;
        p
    }

    #[test]
    fn opts_map_profile_fields() {
        let opts = opts_for(&mysql_profile()).unwrap();
        assert_eq!(opts.ip_or_hostname(), "localhost");
        assert_eq!(opts.tcp_port(), 3306);
        assert_eq!(opts.user(), Some("root"));
        assert_eq!(opts.pass(), Some("pw"));
        assert_eq!(opts.db_name(), Some("appdb"));
    }

    /// Through an SSH tunnel or proxy the driver dials the local end of the
    /// tunnel, but the server's certificate names the server itself.
    #[test]
    fn tunneled_tls_checks_the_certificate_against_the_server_name() {
        let mut p = mysql_profile();
        p.server = "db.internal".into();
        p.tls_mode = TlsMode::VerifyFull;
        let opts = opts_from_profile(&p, "127.0.0.1", 40001).unwrap();
        assert_eq!(opts.ip_or_hostname(), "127.0.0.1");
        assert_eq!(opts.tcp_port(), 40001);
        assert_eq!(
            opts.ssl_opts().unwrap().tls_hostname_override(),
            Some("db.internal")
        );

        let direct = opts_from_profile(&p, "db.internal", 3306).unwrap();
        assert_eq!(direct.ssl_opts().unwrap().tls_hostname_override(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn no_route_to_host_explains_local_network_privacy() {
        let error = mysql_async::Error::from(std::io::Error::from_raw_os_error(65));
        let StudioError::ConnectionMessage(message) = to_connection_error(error) else {
            panic!("a network failure is a connection error");
        };
        assert!(message.fallback().contains("Local Network"), "{message}");
        assert_eq!(
            message.key(),
            Some("services.errors.connection_macos_local_network")
        );
    }

    #[test]
    fn opts_keep_idle_connections_alive() {
        let opts = opts_for(&mysql_profile()).unwrap();
        assert_eq!(opts.tcp_keepalive(), Some(crate::tcp_keepalive::IDLE));
    }

    #[test]
    fn opts_skip_blank_database() {
        let mut p = mysql_profile();
        p.database = Some("".into());
        let opts = opts_for(&p).unwrap();
        assert_eq!(opts.db_name(), None);
    }

    #[test]
    fn non_sql_auth_is_rejected() {
        let mut p = mysql_profile();
        p.authentication = AuthenticationType::Integrated;
        assert!(matches!(opts_for(&p), Err(StudioError::NotImplemented(_))));
        p.authentication = AuthenticationType::EntraDeviceCode;
        assert!(matches!(opts_for(&p), Err(StudioError::InvalidProfile(_))));
    }

    #[test]
    fn value_mapping_scalars() {
        let col = column_stub();
        assert_eq!(value_to_cell(&Value::NULL, &col), CellValue::Null);
        assert_eq!(value_to_cell(&Value::Int(-7), &col), CellValue::I64(-7));
        assert_eq!(value_to_cell(&Value::UInt(7), &col), CellValue::I64(7));
        assert_eq!(
            value_to_cell(&Value::UInt(u64::MAX), &col),
            CellValue::String(u64::MAX.to_string()),
            "u64 overflow degrades to string, not a wrong number"
        );
        assert_eq!(
            value_to_cell(&Value::Double(1.5), &col),
            CellValue::F64(1.5)
        );
    }

    #[test]
    fn text_bytes_become_strings_binary_bytes_stay_bytes() {
        assert_eq!(
            value_to_cell(&Value::Bytes(b"hello".to_vec()), &column_stub()),
            CellValue::String("hello".into())
        );
        assert_eq!(
            value_to_cell(&Value::Bytes(vec![0xDE, 0xAD]), &binary_column_stub()),
            CellValue::Bytes(vec![0xDE, 0xAD])
        );
    }

    #[test]
    fn temporal_values_render_canonically() {
        let col = column_stub();
        assert_eq!(
            value_to_cell(&Value::Date(2026, 7, 19, 0, 0, 0, 0), &col),
            CellValue::String("2026-07-19".into())
        );
        assert_eq!(
            value_to_cell(&Value::Date(2026, 7, 19, 13, 5, 9, 0), &col),
            CellValue::String("2026-07-19 13:05:09".into())
        );
        assert_eq!(
            value_to_cell(&Value::Time(true, 1, 2, 3, 4, 0), &col),
            CellValue::String("-26:03:04".into())
        );
    }

    #[test]
    fn params_map_to_mysql_values() {
        let params = params_to_mysql(&[
            SqlParam::Null,
            SqlParam::Bool(true),
            SqlParam::I64(5),
            SqlParam::String("x".into()),
        ]);
        match params {
            mysql_async::Params::Positional(values) => {
                assert_eq!(values[0], Value::NULL);
                assert_eq!(values[1], Value::Int(1));
                assert_eq!(values[2], Value::Int(5));
                assert_eq!(values[3], Value::Bytes(b"x".to_vec()));
            }
            other => panic!("expected positional params, got {other:?}"),
        }
        assert!(matches!(params_to_mysql(&[]), mysql_async::Params::Empty));
    }

    /// Live test: set SQLLY_MYSQL_SERVER / SQLLY_MYSQL_USER /
    /// SQLLY_MYSQL_PASSWORD (and optionally SQLLY_MYSQL_DATABASE,
    /// SQLLY_MYSQL_PORT) and run with `cargo test -p sqlly-mysql -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a live MySQL server (SQLLY_MYSQL_* env vars)"]
    async fn live_execute_streams_results() {
        let server = std::env::var("SQLLY_MYSQL_SERVER").expect("SQLLY_MYSQL_SERVER");
        let user = std::env::var("SQLLY_MYSQL_USER").expect("SQLLY_MYSQL_USER");
        let mut p = ConnectionProfile::sql_login(
            "mysql-live",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_MYSQL_PASSWORD".into(),
            },
        );
        p.port = std::env::var("SQLLY_MYSQL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3306);
        p.database = std::env::var("SQLLY_MYSQL_DATABASE").ok();
        p.database_type = sqlly_core::DatabaseType::MySql;
        p.encrypt = false;

        let client = open_client(&p).await.unwrap();
        let events = EventBus::new(256);
        let mut sub = events.subscribe();
        execute_on_client(
            &client,
            &p,
            "SELECT 1 AS one; SELECT 'x' AS label;",
            &events,
            None,
        )
        .await
        .unwrap();
        let mut result_sets = 0;
        while let Ok(event) = sub.try_recv() {
            if matches!(
                event.payload,
                sqlly_core::EventPayload::ResultSetStarted { .. }
            ) {
                result_sets += 1;
            }
        }
        assert_eq!(result_sets, 2);
    }
}
