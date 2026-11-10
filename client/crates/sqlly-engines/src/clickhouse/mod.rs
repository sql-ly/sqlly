//! # ClickHouse driver
//!
//! ClickHouse over its HTTP interface (port 8123, or 8443 with TLS),
//! mirroring the other drivers' shape: [`open_client`] /
//! [`execute_on_client`] for the streaming query path,
//! [`query_rows_on_client`] / [`execute_batch_on_client`] for small
//! materialized reads and raw batches, and [`ClickHouseTransport`]
//! implementing `crate::driver::SqlTransport` for the lifecycle service.
//!
//! Every statement is one `POST /` with the SQL as the body and the query
//! settings in the URL: a fresh `query_id` (so Cancel can `KILL QUERY` it
//! from a second request), one `session_id` per client (so `USE`, `SET` and
//! temporary tables carry across the statements of a script), the profile's
//! database, and `JSONCompactEachRowWithNamesAndTypes` as the output format.
//! The response body is consumed as it streams — names line, types line,
//! then one row per line — through the shared [`QueryStreamer`], so large
//! results paint progressively and the bounded row channel applies
//! backpressure. Scripts are split on `;` outside strings and comments and
//! run one statement at a time (the HTTP interface refuses
//! multi-statement bodies); statements without a result set report the
//! written-row count from the `X-ClickHouse-Summary` header.
//!
//! Type mapping lives in [`format`]: integers into `I64` (wider values stay
//! exact text), floats into `F64`, `Bool` into `Bool`, `Decimal`, dates,
//! times, `UUID`, `Enum`, IP addresses into their text form, and
//! `Array`/`Tuple`/`Map`/`Nested` into compact JSON text.
//!
//! Credentials travel as `X-ClickHouse-User` / `X-ClickHouse-Key` headers.
//! TLS follows the profile's [`TlsMode`] with the same rustls configuration
//! (and the same fail-closed rules) as the PostgreSQL driver. SSH tunnels,
//! proxies and Kubernetes port-forwards come from the shared
//! [`crate::ssh::TunnelManager`], exactly like the socket drivers.

#![forbid(unsafe_code)]

pub mod format;

use std::sync::Arc;

use futures::StreamExt;
use sqlly_core::{
    AuthenticationType, CellValue, ConnectionProfile, DatabaseSummary, EventBus,
    MaterializedResultSet, ResultColumn, RowSender, SecretProvider, SecretRef, SqlParam,
    StudioError, StudioResult, TlsMode,
};
use tokio_util::sync::CancellationToken;

use crate::driver::QueryStreamer;
pub use crate::driver::{ConnectionService, SqlTransport};
use format::{
    Line, RESULT_FORMAT, ResponseParser, error_message, is_authentication_error,
    is_query_statement, split_script, written_rows_from_summary,
};

/// The default HTTP port; 8443 is the conventional TLS port.
pub const DEFAULT_PORT: u16 = 8123;

/// A live ClickHouse "connection": an HTTP client plus the session it
/// speaks in. Nothing is held open between statements, so dropping the
/// client costs nothing; the server forgets the session after its timeout.
pub struct ClickHouseClient {
    http: reqwest::Client,
    base_url: String,
    user: String,
    password: String,
    database: Option<String>,
    session_id: String,
    /// The `query_id` of the statement currently streaming, for Cancel.
    running: Arc<parking_lot::Mutex<Option<String>>>,
    cancel: CancellationToken,
}

impl ClickHouseClient {
    /// The base URL requests are sent to (`http://host:8123/`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn request(&self, query_id: &str, wait_end_of_query: bool) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(&self.base_url)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .query(&[
                ("query_id", query_id),
                ("session_id", self.session_id.as_str()),
                ("default_format", RESULT_FORMAT),
                // Wide integers, decimals and NaN/Inf arrive quoted so they
                // survive JSON intact; `format` turns them back into cells.
                ("output_format_json_quote_64bit_integers", "1"),
                ("output_format_json_quote_denormals", "1"),
                ("output_format_json_quote_decimals", "1"),
            ]);
        if let Some(database) = &self.database {
            request = request.query(&[("database", database.as_str())]);
        }
        if wait_end_of_query {
            request = request.query(&[("wait_end_of_query", "1")]);
        }
        request
    }
}

/// Server-side cancellation handle for a [`ClickHouseClient`]: stops the
/// driver's response loop and asks the server to kill the statement it was
/// running (`KILL QUERY WHERE query_id = …`).
#[derive(Clone)]
pub struct ClickHouseCancelHandle {
    http: reqwest::Client,
    base_url: String,
    user: String,
    password: String,
    running: Arc<parking_lot::Mutex<Option<String>>>,
    cancel: CancellationToken,
}

impl ClickHouseCancelHandle {
    /// Cancel: the running statement's id is captured and the driver loop
    /// released synchronously; the returned future sends the `KILL QUERY`.
    pub fn cancel(self) -> impl std::future::Future<Output = StudioResult<()>> + Send + 'static {
        let query_id = self.running.lock().clone();
        self.cancel.cancel();
        async move {
            let Some(query_id) = query_id else {
                return Ok(());
            };
            let sql = format!(
                "KILL QUERY WHERE query_id = '{}' ASYNC",
                query_id.replace('\\', "\\\\").replace('\'', "\\'")
            );
            let response = self
                .http
                .post(&self.base_url)
                .header("X-ClickHouse-User", &self.user)
                .header("X-ClickHouse-Key", &self.password)
                .body(sql)
                .send()
                .await
                .map_err(|e| StudioError::QueryExecution(format!("cancel request failed: {e}")))?;
            let status = response.status().as_u16();
            if status != 200 {
                let body = response.text().await.unwrap_or_default();
                return Err(StudioError::QueryExecution(format!(
                    "cancel request failed: {}",
                    error_message(status, &body)
                )));
            }
            Ok(())
        }
    }
}

/// Obtain a cancellation handle for the client.
pub fn cancel_handle(client: &ClickHouseClient) -> ClickHouseCancelHandle {
    ClickHouseCancelHandle {
        http: client.http.clone(),
        base_url: client.base_url.clone(),
        user: client.user.clone(),
        password: client.password.clone(),
        running: Arc::clone(&client.running),
        cancel: client.cancel.clone(),
    }
}

/// The login the driver sends. Only SQL logins apply; ClickHouse's `default`
/// user is assumed when the username is blank.
fn credentials(profile: &ConnectionProfile) -> StudioResult<(String, String)> {
    match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => {
            let user = if username.trim().is_empty() {
                "default".to_string()
            } else {
                username.trim().to_string()
            };
            let password =
                if secret_ref.key.is_empty() && secret_ref.provider == SecretProvider::InMemory {
                    String::new()
                } else {
                    secret_ref.resolve()?
                };
            Ok((user, password))
        }
        AuthenticationType::Integrated => Err(StudioError::NotImplemented(
            "Integrated authentication is not supported for ClickHouse connections",
        )),
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            Err(StudioError::InvalidProfile(
                "Microsoft Entra authentication applies to SQL Server, not ClickHouse".into(),
            ))
        }
        AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. } => {
            Err(StudioError::InvalidProfile(
                "Cloud IAM sign-in is not available for ClickHouse connections".into(),
            ))
        }
        AuthenticationType::AuthToken { .. } => Err(StudioError::InvalidProfile(
            "Auth-token sign-in applies to libSQL / Turso, not ClickHouse".into(),
        )),
    }
}

/// The URL the driver will POST to for a profile: `http` when the profile's
/// TLS mode is `Disable`, `https` otherwise. IPv6 hosts are bracketed.
pub fn base_url_for(profile: &ConnectionProfile, host: &str, port: u16) -> String {
    let scheme = if profile.effective_tls_mode() == TlsMode::Disable {
        "http"
    } else {
        "https"
    };
    let host = host.trim();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = if port == 0 { DEFAULT_PORT } else { port };
    format!("{scheme}://{host}:{port}/")
}

/// Open a client for the profile and verify it with `SELECT version()`.
/// Fails closed like the socket drivers: an encrypted profile whose
/// certificate does not validate errors instead of downgrading.
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<ClickHouseClient> {
    let (host, port) = crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    let (user, password) = credentials(profile)?;
    let mode = profile.effective_tls_mode();
    // reqwest is built without a crypto provider of its own, and the
    // process-level default is not installed in tests or the CLI, so the
    // connector always gets an explicit ring-backed rustls config — even for
    // plaintext profiles, where it simply goes unused.
    let tls_mode = match mode {
        TlsMode::Disable | TlsMode::Legacy => TlsMode::VerifyFull,
        explicit => explicit,
    };
    let tls = crate::pg::rustls_config(profile, tls_mode)?;
    let http = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(profile.timeout())
        .build()
        .map_err(|e| StudioError::Connection(format!("cannot build the HTTP client: {e}")))?;
    let client = ClickHouseClient {
        http,
        base_url: base_url_for(profile, &host, port),
        user,
        password,
        database: profile
            .database
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        session_id: uuid::Uuid::new_v4().to_string(),
        running: Arc::new(parking_lot::Mutex::new(None)),
        cancel: CancellationToken::new(),
    };
    let timeout = profile.timeout();
    let probe = tokio::time::timeout(timeout, fetch(&client, "SELECT version()", false))
        .await
        .map_err(|_| {
            StudioError::Connection(format!(
                "timed out connecting to {} after {:?}",
                client.base_url, timeout
            ))
        })?;
    match probe {
        Ok(_) => {}
        Err(StudioError::QueryExecution(text)) => {
            return Err(StudioError::Connection(text));
        }
        Err(other) => return Err(other),
    }
    apply_startup_sql(&client, profile).await?;
    Ok(client)
}

async fn apply_startup_sql(
    client: &ClickHouseClient,
    profile: &ConnectionProfile,
) -> StudioResult<()> {
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

fn transport_error(client: &ClickHouseClient, error: reqwest::Error) -> StudioError {
    if error.is_timeout() {
        return StudioError::Connection(format!(
            "timed out connecting to {}: {error}",
            client.base_url
        ));
    }
    if error.is_connect() || error.is_request() {
        return StudioError::Connection(format!("cannot reach {}: {error}", client.base_url));
    }
    StudioError::QueryExecution(format!("ClickHouse response ended early: {error}"))
}

fn response_error(status: u16, body: &str) -> StudioError {
    let text = error_message(status, body);
    if is_authentication_error(status, body) {
        StudioError::Authentication(text)
    } else {
        StudioError::QueryExecution(text)
    }
}

/// A fully read statement response.
#[derive(Debug, Default)]
struct Fetched {
    columns: Option<Vec<ResultColumn>>,
    rows: Vec<Vec<CellValue>>,
    text: Vec<String>,
    written_rows: u64,
}

/// Run one statement and read its whole response (small reads: catalog
/// probes, `list_databases`, raw batches).
async fn fetch(
    client: &ClickHouseClient,
    text: &str,
    wait_end_of_query: bool,
) -> StudioResult<Fetched> {
    let query_id = uuid::Uuid::new_v4().to_string();
    let response = client
        .request(&query_id, wait_end_of_query)
        .body(text.to_string())
        .send()
        .await
        .map_err(|e| transport_error(client, e))?;
    let status = response.status().as_u16();
    let summary = summary_header(&response);
    let body = response
        .bytes()
        .await
        .map_err(|e| transport_error(client, e))?;
    if status != 200 {
        return Err(response_error(status, &String::from_utf8_lossy(&body)));
    }
    let mut parser = ResponseParser::new();
    let mut fetched = Fetched {
        written_rows: summary
            .as_deref()
            .and_then(written_rows_from_summary)
            .unwrap_or(0),
        ..Fetched::default()
    };
    let mut lines = parser.push_bytes(&body);
    lines.extend(parser.finish());
    for line in lines {
        match line {
            Line::Header(columns) => fetched.columns = Some(columns),
            Line::Row(cells) => fetched.rows.push(cells),
            Line::ServerError(message) => return Err(StudioError::QueryExecution(message)),
            Line::Text(text) => fetched.text.push(text),
        }
    }
    Ok(fetched)
}

fn summary_header(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("X-ClickHouse-Summary")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Materialized read of one statement, no safety policy. Statements that
/// return no rows yield an empty result set.
pub async fn query_rows_on_client(
    client: &ClickHouseClient,
    sql: &str,
) -> StudioResult<MaterializedResultSet> {
    let fetched = fetch(client, sql.trim(), !is_query_statement(sql)).await?;
    Ok(MaterializedResultSet {
        columns: fetched.columns.unwrap_or_default(),
        rows: fetched.rows,
    })
}

/// The first cell of the first row, as text, or `None` when the statement
/// returned nothing.
pub async fn query_scalar_on_client(
    client: &ClickHouseClient,
    sql: &str,
) -> StudioResult<Option<String>> {
    let set = query_rows_on_client(client, sql).await?;
    Ok(set
        .rows
        .first()
        .and_then(|row| row.first())
        .map(|cell| match cell {
            CellValue::String(s) => s.clone(),
            CellValue::I64(n) => n.to_string(),
            CellValue::F64(f) => f.to_string(),
            CellValue::Bool(b) => b.to_string(),
            CellValue::Null => String::new(),
            CellValue::Bytes(b) => format!("{} bytes", b.len()),
        }))
}

/// Run a raw script statement by statement with no safety policy and no
/// result decoding; the first failing statement ends the batch.
pub async fn execute_batch_on_client(client: &ClickHouseClient, sql: &str) -> StudioResult<()> {
    for statement in split_script(sql) {
        fetch(client, &statement, !is_query_statement(&statement)).await?;
    }
    Ok(())
}

/// Run a statement, returning the written-row count from the summary
/// header (INSERT / ALTER … UPDATE / DELETE); 0 for DDL.
pub async fn execute_statement_on_client(
    client: &ClickHouseClient,
    sql: &str,
) -> StudioResult<u64> {
    let fetched = fetch(client, sql.trim(), true).await?;
    Ok(fetched.written_rows)
}

/// Run one statement whose `?` placeholders (outside quotes) are bound to
/// `params`, returning the written-row count. The HTTP interface has no
/// client-side parameter binding for VALUES lists, so each parameter is
/// rendered as a ClickHouse literal — strings with `\\` and `'` escaped,
/// bytes as `unhex('…')`, booleans as `true` / `false`, NULL as `NULL`.
pub async fn execute_params_on_client(
    client: &ClickHouseClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<u64> {
    let bound = bind_params(sql, params)?;
    execute_statement_on_client(client, &bound).await
}

/// Splice `params` into the `?` placeholders of `sql`, skipping `?` inside
/// single-quoted strings, double-quoted and backtick-quoted identifiers.
/// ClickHouse's lexer honours backslash escapes and doubled closers in all
/// three quote kinds, so the scanner does too — a disagreement would splice
/// a value inside a quoted name and leave the real placeholder behind.
/// The placeholder and parameter counts must match exactly.
pub fn bind_params(sql: &str, params: &[SqlParam]) -> StudioResult<String> {
    let mut out = String::with_capacity(sql.len() + params.len() * 8);
    let mut next = params.iter();
    let mut quote: Option<char> = None;
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                out.push(ch);
                if ch == '\\' {
                    // Backslash escapes the next character inside any quote.
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else if ch == q {
                    if chars.peek() == Some(&q) {
                        // A doubled quote stays inside the literal.
                        out.push(chars.next().unwrap_or(q));
                    } else {
                        quote = None;
                    }
                }
            }
            None => match ch {
                '\'' | '"' | '`' => {
                    quote = Some(ch);
                    out.push(ch);
                }
                '?' => match next.next() {
                    Some(param) => out.push_str(&param_literal(param)),
                    None => {
                        return Err(StudioError::QueryExecution(
                            "more '?' placeholders than parameters".into(),
                        ));
                    }
                },
                other => out.push(other),
            },
        }
    }
    if next.next().is_some() {
        return Err(StudioError::QueryExecution(
            "more parameters than '?' placeholders".into(),
        ));
    }
    Ok(out)
}

/// One parameter as a ClickHouse literal.
pub fn param_literal(param: &SqlParam) -> String {
    match param {
        SqlParam::Null => "NULL".to_string(),
        SqlParam::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        SqlParam::I64(n) => n.to_string(),
        SqlParam::F64(f) => {
            if f.is_nan() {
                "nan".to_string()
            } else if f.is_infinite() {
                if *f > 0.0 { "inf" } else { "-inf" }.to_string()
            } else {
                format!("{f:?}")
            }
        }
        SqlParam::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
        SqlParam::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            format!("unhex('{hex}')")
        }
    }
}

/// Run a script on an open client, streaming results through the shared
/// driver contract (events on the bus, rows on the bounded channel).
/// Applies the profile's safety policy; the rollback wrap is refused up
/// front because ClickHouse has no transactions to roll back.
pub async fn execute_on_client(
    client: &ClickHouseClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    let sql = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    for statement in split_script(&sql) {
        if client.cancel.is_cancelled() {
            return Err(StudioError::QueryCancelled);
        }
        let query_id = uuid::Uuid::new_v4().to_string();
        *client.running.lock() = Some(query_id.clone());
        let outcome = stream_statement(client, &statement, &query_id, &mut streamer).await;
        *client.running.lock() = None;
        if !outcome? {
            // The row consumer went away: stop without running the rest.
            break;
        }
    }
    streamer.finish().await
}

/// Stream one statement. `Ok(false)` means the consumer dropped its
/// receiver and the script should stop.
async fn stream_statement(
    client: &ClickHouseClient,
    text: &str,
    query_id: &str,
    streamer: &mut QueryStreamer<'_>,
) -> StudioResult<bool> {
    let wait_end_of_query = !is_query_statement(text);
    let send = client
        .request(query_id, wait_end_of_query)
        .body(text.to_string())
        .send();
    let response = tokio::select! {
        biased;
        _ = client.cancel.cancelled() => return Err(StudioError::QueryCancelled),
        response = send => response.map_err(|e| transport_error(client, e))?,
    };
    let status = response.status().as_u16();
    let summary = summary_header(&response);
    if status != 200 {
        let body = response.text().await.unwrap_or_default();
        return Err(response_error(status, &body));
    }
    let stream = response.bytes_stream();
    tokio::pin!(stream);
    let mut parser = ResponseParser::new();
    let mut text_lines: Vec<String> = Vec::new();
    loop {
        // Race the next chunk against the pending batch's flush deadline so
        // a slow query still surfaces buffered rows on time, and against
        // Cancel so the loop releases the response immediately.
        let chunk = tokio::select! {
            biased;
            _ = client.cancel.cancelled() => return Err(StudioError::QueryCancelled),
            chunk = stream.next() => chunk,
            _ = flush_deadline(streamer.pending_flush_deadline()) => {
                streamer.flush_now().await;
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|e| transport_error(client, e))?;
        for line in parser.push_bytes(&chunk) {
            if !handle_line(line, streamer, &mut text_lines).await? {
                return Ok(false);
            }
        }
    }
    if let Some(line) = parser.finish()
        && !handle_line(line, streamer, &mut text_lines).await?
    {
        return Ok(false);
    }
    if parser.has_header() {
        return Ok(true);
    }
    if text_lines.is_empty() {
        let written = summary
            .as_deref()
            .and_then(written_rows_from_summary)
            .unwrap_or(0);
        streamer.statement_completed(written)?;
        return Ok(true);
    }
    // A statement that chose its own FORMAT (or plain-text output): show
    // the lines as one text column rather than dropping them.
    streamer
        .begin_result_set(vec![ResultColumn {
            name: "output".to_string(),
            data_type: "String".to_string(),
            nullable: false,
        }])
        .await?;
    for line in text_lines {
        if !streamer.push_row(vec![CellValue::String(line)]).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn flush_deadline(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending::<()>().await,
    }
}

async fn handle_line(
    line: Line,
    streamer: &mut QueryStreamer<'_>,
    text_lines: &mut Vec<String>,
) -> StudioResult<bool> {
    match line {
        Line::Header(columns) => {
            streamer.begin_result_set(columns).await?;
            Ok(true)
        }
        Line::Row(cells) => streamer.push_row(cells).await,
        Line::ServerError(message) => Err(StudioError::QueryExecution(message)),
        Line::Text(text) => {
            text_lines.push(text);
            Ok(true)
        }
    }
}

/// `SqlTransport` implementation for ClickHouse, usable with the shared
/// [`ConnectionService`].
#[derive(Debug, Clone, Default)]
pub struct ClickHouseTransport;

#[async_trait::async_trait]
impl SqlTransport for ClickHouseTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        let set = query_rows_on_client(&client, "SHOW DATABASES").await?;
        Ok(set
            .rows
            .into_iter()
            .filter_map(|row| match row.first() {
                Some(CellValue::String(name)) => Some(DatabaseSummary {
                    name: name.clone(),
                    state: "ONLINE".to_string(),
                    compatibility_level: None,
                }),
                _ => None,
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
        execute_on_client(&client, profile, sql, events, rows_tx).await
    }
}

/// Build a ClickHouse profile from a URL of the form
/// `http[s]://user:password@host:port/database` — the shape the
/// `SQLLY_CLICKHOUSE_URL` live-test variable uses. Missing pieces fall back
/// to the `default` user, no password, port 8123 and no database; `https`
/// turns encryption on with the server certificate trusted (a lab server
/// rarely has a public certificate).
pub fn profile_from_url(name: &str, url: &str) -> StudioResult<ConnectionProfile> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|e| StudioError::InvalidProfile(format!("invalid ClickHouse URL: {e}")))?;
    let https = match parsed.scheme() {
        "http" | "clickhouse" => false,
        "https" | "clickhouses" => true,
        other => {
            return Err(StudioError::InvalidProfile(format!(
                "unsupported ClickHouse URL scheme '{other}'"
            )));
        }
    };
    let host = parsed.host_str().ok_or_else(|| {
        StudioError::InvalidProfileMessage(sqlly_core::DisplayMessage::new(
            "services.clickhouse.errors.no_host",
            "ClickHouse URL has no host",
        ))
    })?;
    let username = if parsed.username().is_empty() {
        "default".to_string()
    } else {
        percent_decode(parsed.username())
    };
    let password = parsed.password().map(percent_decode).unwrap_or_default();
    let mut profile = ConnectionProfile::sql_login(
        name,
        host,
        username,
        SecretRef {
            provider: SecretProvider::InMemory,
            key: password,
        },
    );
    profile.database_type = sqlly_core::DatabaseType::ClickHouse;
    profile.port = parsed.port().unwrap_or(DEFAULT_PORT);
    profile.database = Some(parsed.path().trim_matches('/').to_string()).filter(|s| !s.is_empty());
    profile.encrypt = https;
    profile.trust_server_certificate = https;
    profile.timeout_seconds = profile.timeout_seconds.max(30);
    Ok(profile)
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The live-test profile from `SQLLY_CLICKHOUSE_URL`, or `None` so tests
/// self-skip on a checkout without a server.
pub fn live_profile_from_env(name: &str) -> Option<ConnectionProfile> {
    let url = std::env::var("SQLLY_CLICKHOUSE_URL").ok()?;
    match profile_from_url(name, &url) {
        Ok(profile) => Some(profile),
        Err(error) => {
            eprintln!("SQLLY_CLICKHOUSE_URL is not usable: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ClickHouse reads backslash escapes inside `"…"` and `` `…` `` quoted
    /// names too: the placeholder scanner must agree, or a `?` inside a
    /// quoted name is spliced and the real placeholder is left behind.
    #[test]
    fn params_skip_placeholders_inside_backslash_escaped_identifiers() {
        let sql = "SELECT \"a\\\"?\", `b\\`?` FROM t WHERE x = ?";
        let bound = bind_params(sql, &[SqlParam::I64(7)]).expect("one placeholder");
        assert_eq!(bound, "SELECT \"a\\\"?\", `b\\`?` FROM t WHERE x = 7");
    }
    use sqlly_core::DatabaseType;
    use std::time::Duration;

    #[test]
    fn params_bind_as_literals_outside_quotes() {
        let sql = "INSERT INTO `we?ird` (`a`, \"b?\", c) VALUES (?, 'x?y', ?, ?, ?, ?)";
        let bound = bind_params(
            sql,
            &[
                SqlParam::I64(7),
                SqlParam::String("o'b\\c".into()),
                SqlParam::Null,
                SqlParam::Bool(true),
                SqlParam::Bytes(vec![0xDE, 0xAD]),
            ],
        )
        .unwrap();
        assert_eq!(
            bound,
            "INSERT INTO `we?ird` (`a`, \"b?\", c) VALUES (7, 'x?y', 'o''b\\\\c', NULL, true, unhex('DEAD'))"
        );
        assert_eq!(param_literal(&SqlParam::F64(1.5)), "1.5");
        assert_eq!(param_literal(&SqlParam::F64(2.0)), "2.0");
        assert_eq!(param_literal(&SqlParam::F64(f64::NAN)), "nan");
        assert_eq!(param_literal(&SqlParam::F64(f64::NEG_INFINITY)), "-inf");
        // A doubled quote and a backslash-escaped quote both stay inside the
        // literal, so the `?` after them is still bound.
        let tricky = bind_params("SELECT 'it''s ?', 'a\\'? ', ?", &[SqlParam::I64(1)]).unwrap();
        assert_eq!(tricky, "SELECT 'it''s ?', 'a\\'? ', 1");
        assert!(bind_params("VALUES (?, ?)", &[SqlParam::I64(1)]).is_err());
        assert!(bind_params("VALUES (?)", &[SqlParam::I64(1), SqlParam::I64(2)]).is_err());
    }

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "ch",
            "ch.example.com",
            "analyst",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        p.database_type = DatabaseType::ClickHouse;
        p.port = 8123;
        p
    }

    #[test]
    fn base_url_follows_tls_mode_and_brackets_ipv6() {
        let mut p = profile();
        p.encrypt = false;
        assert_eq!(
            base_url_for(&p, "ch.example.com", 8123),
            "http://ch.example.com:8123/"
        );
        p.encrypt = true;
        assert_eq!(
            base_url_for(&p, "ch.example.com", 8443),
            "https://ch.example.com:8443/"
        );
        p.tls_mode = TlsMode::Disable;
        assert_eq!(base_url_for(&p, "::1", 0), "http://[::1]:8123/");
        p.tls_mode = TlsMode::Require;
        assert_eq!(base_url_for(&p, "[::1]", 9000), "https://[::1]:9000/");
    }

    #[test]
    fn credentials_default_the_user_and_reject_foreign_auth() {
        let (user, password) = credentials(&profile()).unwrap();
        assert_eq!((user.as_str(), password.as_str()), ("analyst", "pw"));
        let mut blank = profile();
        blank.authentication = AuthenticationType::SqlLogin {
            username: String::new(),
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        };
        assert_eq!(
            credentials(&blank).unwrap(),
            ("default".to_string(), String::new())
        );
        let mut entra = profile();
        entra.authentication = AuthenticationType::EntraBrowser;
        assert!(matches!(
            credentials(&entra),
            Err(StudioError::InvalidProfile(_))
        ));
        let mut integrated = profile();
        integrated.authentication = AuthenticationType::Integrated;
        assert!(matches!(
            credentials(&integrated),
            Err(StudioError::NotImplemented(_))
        ));
    }

    #[test]
    fn profile_from_url_fills_every_field() {
        let p =
            profile_from_url("live", "http://default:s%40lly@127.0.0.1:8123/analytics").unwrap();
        assert_eq!(p.database_type, DatabaseType::ClickHouse);
        assert_eq!(p.server, "127.0.0.1");
        assert_eq!(p.port, 8123);
        assert_eq!(p.database.as_deref(), Some("analytics"));
        assert!(!p.encrypt);
        match &p.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "default");
                assert_eq!(secret_ref.key, "s@lly");
            }
            other => panic!("{other:?}"),
        }
        let bare = profile_from_url("live", "https://ch.example.com").unwrap();
        assert_eq!(bare.port, DEFAULT_PORT);
        assert!(bare.database.is_none());
        assert!(bare.encrypt && bare.trust_server_certificate);
        assert!(profile_from_url("x", "mysql://h").is_err());
        assert!(profile_from_url("x", "not a url").is_err());
    }

    #[test]
    fn response_errors_classify_authentication() {
        assert!(matches!(
            response_error(
                403,
                "Code: 516. DB::Exception: default: Authentication failed (AUTHENTICATION_FAILED)"
            ),
            StudioError::Authentication(_)
        ));
        assert!(matches!(
            response_error(400, "Code: 62. DB::Exception: Syntax error"),
            StudioError::QueryExecution(text) if text.starts_with("Code: 62")
        ));
    }

    /// Live: connect, list databases, stream 100k rows progressively, map
    /// every type family, count DDL/INSERT rows, and cancel a slow query.
    /// Opt-in through `SQLLY_CLICKHOUSE_URL`
    /// (`http://default:sqlly@127.0.0.1:8123/default`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_streams_types_counts_and_cancels() {
        let Some(profile) = live_profile_from_env("clickhouse-live") else {
            eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
            return;
        };
        let transport = ClickHouseTransport;
        transport.test_connection(&profile).await.unwrap();
        let dbs = transport.list_databases(&profile).await.unwrap();
        assert!(dbs.iter().any(|d| d.name == "system"), "{dbs:?}");

        // Bad password surfaces as an authentication error with the
        // server's own text.
        let mut wrong = profile.clone();
        wrong.authentication = AuthenticationType::SqlLogin {
            username: "default".into(),
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: "definitely-wrong".into(),
            },
        };
        match open_client(&wrong).await {
            Err(StudioError::Authentication(text)) => assert!(text.contains("Authentication")),
            Err(other) => panic!("expected an authentication error, got {other:?}"),
            Ok(_) => panic!("a wrong password must not connect"),
        }

        let client = open_client(&profile).await.unwrap();

        // 100k rows stream through the bounded channel in several batches.
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let (tx, mut rx) = sqlly_core::row_channel(8);
        let drain = tokio::spawn(async move {
            let mut total = 0usize;
            let mut batches = 0usize;
            while let Some(batch) = rx.recv().await {
                total += batch.rows.len();
                batches += 1;
            }
            (total, batches)
        });
        execute_on_client(
            &client,
            &profile,
            "SELECT number FROM numbers(100000)",
            &events,
            Some(&tx),
        )
        .await
        .unwrap();
        drop(tx);
        let (total, batches) = drain.await.unwrap();
        assert_eq!(total, 100_000);
        assert!(
            batches > 100,
            "rows must arrive in many batches, got {batches}"
        );
        let mut saw_columns = false;
        while let Ok(event) = sub.try_recv() {
            if let sqlly_core::EventPayload::ResultSetStarted { columns, .. } = event.payload {
                assert_eq!(columns[0].name, "number");
                assert_eq!(columns[0].data_type, "UInt64");
                saw_columns = true;
            }
        }
        assert!(saw_columns);

        // Every type family maps into the grid.
        let set = query_rows_on_client(
            &client,
            "SELECT toInt8(-1) AS i8, toUInt64(18446744073709551615) AS u64, toInt128(-7) AS i128, \
             toFloat64(1.5) AS f, toDecimal64(-1.25, 4) AS d, 'x' AS s, toFixedString('ab', 4) AS fs, \
             toDate('2020-01-02') AS dt, toDateTime64('2020-01-01 10:11:12.123', 3, 'UTC') AS dt64, \
             toUUID('61f0c404-5cb3-11e7-907b-a6006ad3dba0') AS u, true AS b, \
             CAST('x', 'Enum8(''x'' = 1, ''y'' = 2)') AS e, toNullable(NULL) AS n, \
             CAST('a', 'LowCardinality(String)') AS lc, [1, 2] AS arr, (1, 'a') AS tup, \
             map('k', 1) AS m, toIPv4('1.2.3.4') AS ip4, toIPv6('::1') AS ip6, 0/0 AS nan",
        )
        .await
        .unwrap();
        let row = &set.rows[0];
        let by_name = |name: &str| {
            let idx = set.columns.iter().position(|c| c.name == name).unwrap();
            row[idx].clone()
        };
        assert_eq!(by_name("i8"), CellValue::I64(-1));
        assert_eq!(
            by_name("u64"),
            CellValue::String("18446744073709551615".into())
        );
        assert_eq!(by_name("i128"), CellValue::I64(-7));
        assert_eq!(by_name("f"), CellValue::F64(1.5));
        assert_eq!(by_name("d"), CellValue::String("-1.25".into()));
        assert_eq!(by_name("s"), CellValue::String("x".into()));
        assert_eq!(by_name("fs"), CellValue::String("ab\u{0}\u{0}".into()));
        assert_eq!(by_name("dt"), CellValue::String("2020-01-02".into()));
        assert_eq!(
            by_name("dt64"),
            CellValue::String("2020-01-01 10:11:12.123".into())
        );
        assert_eq!(
            by_name("u"),
            CellValue::String("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into())
        );
        assert_eq!(by_name("b"), CellValue::Bool(true));
        assert_eq!(by_name("e"), CellValue::String("x".into()));
        assert_eq!(by_name("n"), CellValue::Null);
        assert_eq!(by_name("lc"), CellValue::String("a".into()));
        assert_eq!(by_name("arr"), CellValue::String("[1,2]".into()));
        assert_eq!(by_name("tup"), CellValue::String("[1,\"a\"]".into()));
        assert_eq!(by_name("m"), CellValue::String("{\"k\":1}".into()));
        assert_eq!(by_name("ip4"), CellValue::String("1.2.3.4".into()));
        assert_eq!(by_name("ip6"), CellValue::String("::1".into()));
        assert_eq!(by_name("nan"), CellValue::String("nan".into()));
        let nullable = set.columns.iter().find(|c| c.name == "n").unwrap();
        assert!(nullable.nullable);

        // DDL reports 0, INSERT reports the written rows, a script mixes
        // result sets and counts in order.
        let table = format!("sqlly_live_{}", uuid::Uuid::new_v4().simple());
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        execute_on_client(
            &client,
            &profile,
            &format!(
                "CREATE TABLE {table} (id UInt32, v String) ENGINE = MergeTree ORDER BY id;\n\
                 INSERT INTO {table} SELECT number, toString(number) FROM numbers(1234);\n\
                 SELECT count() AS n FROM {table};\n\
                 DROP TABLE {table}"
            ),
            &events,
            None,
        )
        .await
        .unwrap();
        let mut counts = Vec::new();
        let mut rows = Vec::new();
        while let Ok(event) = sub.try_recv() {
            match event.payload {
                sqlly_core::EventPayload::RowCount { rows_affected, .. } => {
                    counts.push(rows_affected)
                }
                sqlly_core::EventPayload::Rows { rows: r, .. } => rows.extend(r),
                _ => {}
            }
        }
        // The streamer closes a result set (its RowCount) when the next
        // statement reports, so the SELECT's count lands after the DROP's.
        assert_eq!(counts, vec![0, 1234, 0, 1], "{counts:?}");
        assert_eq!(rows, vec![vec![CellValue::I64(1234)]]);

        // A syntax error carries ClickHouse's own message.
        let bus = EventBus::new(8);
        let _keep = bus.subscribe();
        let err = execute_on_client(&client, &profile, "SELECT bad syntax here", &bus, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Syntax error"), "{err}");

        // Cancel: a 30 s query stops within a couple of seconds.
        let slow = open_client(&profile).await.unwrap();
        let handle = cancel_handle(&slow);
        let started = std::time::Instant::now();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            handle.cancel().await
        });
        let result = execute_on_client(
            &slow,
            &profile,
            "SELECT sleepEachRow(1) FROM numbers(30) SETTINGS max_block_size = 1",
            &bus,
            None,
        )
        .await;
        assert!(
            matches!(result, Err(StudioError::QueryCancelled)),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        canceller.await.unwrap().unwrap();
        // The server no longer runs it.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let running = query_scalar_on_client(
            &client,
            "SELECT count() FROM system.processes WHERE query LIKE '%sleepEachRow(1)%' AND query NOT LIKE '%system.processes%'",
        )
        .await
        .unwrap();
        assert_eq!(running.as_deref(), Some("0"));
    }
}
