//! # libSQL / Turso driver
//!
//! SQLite's dialect served over the network by `sqld` (self-hosted libSQL
//! server, port 8080) and Turso Cloud (HTTPS on 443), spoken through the
//! Hrana v2 HTTP pipeline (`POST /v2/pipeline`). Mirrors the other drivers'
//! shape: [`open_client`] / [`execute_on_client`] for the query path,
//! [`query_rows_on_client`] / [`execute_batch_on_client`] for small
//! materialized reads and raw batches, and [`LibSqlTransport`] implementing
//! `crate::driver::SqlTransport` for the lifecycle service.
//!
//! A script is one HTTP request: its statements are split locally (the
//! server refuses a request holding two) and sent as one Hrana `batch`
//! whose steps each depend on the previous step's success, on a single
//! server-side stream — so `BEGIN … COMMIT` and the rollback-wrap policy
//! work, and the first failing statement stops the script. Hrana has no
//! streaming of a single result set: the whole response is read, then fed
//! through the shared [`QueryStreamer`] in row batches so the grid paints
//! progressively; very large results therefore cost their full size in
//! memory before the first row appears (page with `LIMIT`/`OFFSET`).
//! Cancel drops the in-flight request; the server has no kill primitive, so
//! a long statement runs to completion there.
//!
//! Type mapping lives in [`hrana`]: integers into `I64` (exact text when
//! wider), floats into `F64`, text into text, blobs into bytes, with the
//! column's declared type captured like the in-process SQLite driver.
//!
//! Credentials: a bearer token (`Authorization: Bearer …`) from the
//! profile's [`AuthenticationType::AuthToken`] reference, resolved through
//! the keyring like a password; no token means an anonymous local server.
//! TLS follows the profile's [`TlsMode`] with the PostgreSQL driver's
//! rustls configuration, and a `*.turso.io` host is always HTTPS so a token
//! never travels in the clear. SSH tunnels, proxies and Kubernetes
//! port-forwards come from the shared [`crate::ssh::TunnelManager`].

#![forbid(unsafe_code)]

pub mod hrana;

use serde_json::Value;
use sqlly_core::{
    AuthenticationType, CellValue, ConnectionProfile, DatabaseSummary, EventBus,
    MaterializedResultSet, RowSender, SecretProvider, SecretRef, StudioError, StudioResult,
    TlsMode,
};
use tokio_util::sync::CancellationToken;

use crate::driver::QueryStreamer;
pub use crate::driver::{ConnectionService, SqlTransport};
use hrana::{
    BatchOutcome, PIPELINE_PATH, PipelineResponse, StatementOutcome, StreamResponse, StreamResult,
    batch_pipeline, batch_request, decode_batch, decode_statement, error_message, execute_pipeline,
    execute_request, is_authentication_error, pipeline_body, split_script,
};
use sqlly_core::SqlParam;

/// `sqld`'s own port; Turso Cloud serves every database on HTTPS 443.
pub const DEFAULT_PORT: u16 = 8080;
/// The port a TLS profile without an explicit port dials.
pub const TLS_PORT: u16 = 443;

/// A live libSQL "connection": an HTTP client, the pipeline URL, and the
/// bearer token. Nothing is held open between scripts; a
/// [`LibSqlStream`] keeps one server-side stream (and so one transaction)
/// open across requests when a caller needs that. Cloning shares the HTTP
/// connection pool and the cancellation token.
#[derive(Clone)]
pub struct LibSqlClient {
    http: reqwest::Client,
    pipeline_url: String,
    token: Option<String>,
    cancel: CancellationToken,
}

impl LibSqlClient {
    /// The pipeline URL requests are posted to.
    pub fn pipeline_url(&self) -> &str {
        &self.pipeline_url
    }

    fn request_at(&self, url: &str) -> reqwest::RequestBuilder {
        let mut request = self.http.post(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request
    }
}

/// Cancellation handle for a [`LibSqlClient`]: releases the in-flight
/// request. Hrana has no server-side kill, so the statement itself finishes
/// on the server.
#[derive(Clone)]
pub struct LibSqlCancelHandle {
    cancel: CancellationToken,
}

impl LibSqlCancelHandle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Obtain a cancellation handle for the client.
pub fn cancel_handle(client: &LibSqlClient) -> LibSqlCancelHandle {
    LibSqlCancelHandle {
        cancel: client.cancel.clone(),
    }
}

/// The bearer token the driver sends, or `None` for an anonymous server.
/// A SQL-login password doubles as the token so a `--password` from the CLI
/// or an older profile still signs in.
fn bearer_token(profile: &ConnectionProfile) -> StudioResult<Option<String>> {
    let resolve = |secret_ref: &SecretRef| -> StudioResult<Option<String>> {
        if secret_ref.provider == SecretProvider::InMemory && secret_ref.key.trim().is_empty() {
            return Ok(None);
        }
        let token = secret_ref.resolve()?;
        Ok(Some(token.trim().to_string()).filter(|t| !t.is_empty()))
    };
    match &profile.authentication {
        AuthenticationType::AuthToken { secret_ref } => resolve(secret_ref),
        AuthenticationType::SqlLogin { secret_ref, .. } => resolve(secret_ref),
        AuthenticationType::Integrated => Err(StudioError::NotImplemented(
            "Integrated authentication is not supported for libSQL connections",
        )),
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            Err(StudioError::InvalidProfile(
                "Microsoft Entra authentication applies to SQL Server, not libSQL".into(),
            ))
        }
        AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. } => {
            Err(StudioError::InvalidProfile(
                "Cloud IAM sign-in is not available for libSQL connections".into(),
            ))
        }
    }
}

/// Whether a host is Turso Cloud, which only speaks HTTPS.
pub fn is_turso_host(host: &str) -> bool {
    host.trim().to_ascii_lowercase().ends_with(".turso.io")
}

/// The pipeline URL the driver will POST to: `https` when the profile's TLS
/// mode is anything but `Disable` or the host is Turso Cloud, `http`
/// otherwise; a zero port becomes 443 or 8080 accordingly. IPv6 hosts are
/// bracketed.
pub fn pipeline_url_for(profile: &ConnectionProfile, host: &str, port: u16) -> String {
    let https = profile.effective_tls_mode() != TlsMode::Disable || is_turso_host(host);
    let scheme = if https { "https" } else { "http" };
    let host = host.trim();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = match port {
        0 if https => TLS_PORT,
        0 => DEFAULT_PORT,
        explicit => explicit,
    };
    format!("{scheme}://{host}:{port}/{PIPELINE_PATH}")
}

/// Open a client for the profile and verify it with `SELECT
/// sqlite_version()`. Fails closed like the socket drivers: an encrypted
/// profile whose certificate does not validate errors instead of
/// downgrading.
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<LibSqlClient> {
    let (host, port) = crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    let token = bearer_token(profile)?;
    // reqwest is built without a crypto provider of its own, so the
    // connector always gets an explicit ring-backed rustls config — even
    // for plaintext profiles, where it simply goes unused.
    let tls_mode = match profile.effective_tls_mode() {
        TlsMode::Disable | TlsMode::Legacy => TlsMode::VerifyFull,
        explicit => explicit,
    };
    let tls = crate::pg::rustls_config(profile, tls_mode)?;
    let http = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(profile.timeout())
        .build()
        .map_err(|e| StudioError::Connection(format!("cannot build the HTTP client: {e}")))?;
    let client = LibSqlClient {
        http,
        pipeline_url: pipeline_url_for(profile, &host, port),
        token,
        cancel: CancellationToken::new(),
    };
    let timeout = profile.timeout();
    let probe = tokio::time::timeout(
        timeout,
        query_scalar_on_client(&client, "SELECT sqlite_version()"),
    )
    .await
    .map_err(|_| {
        StudioError::Connection(format!(
            "timed out connecting to {} after {:?}",
            client.pipeline_url, timeout
        ))
    })?;
    match probe {
        Ok(_) => {}
        Err(StudioError::QueryExecution(text)) => return Err(StudioError::Connection(text)),
        Err(other) => return Err(other),
    }
    apply_startup_sql(&client, profile).await?;
    Ok(client)
}

async fn apply_startup_sql(client: &LibSqlClient, profile: &ConnectionProfile) -> StudioResult<()> {
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

fn transport_error(client: &LibSqlClient, error: reqwest::Error) -> StudioError {
    if error.is_timeout() {
        return StudioError::Connection(format!(
            "timed out connecting to {}: {error}",
            client.pipeline_url
        ));
    }
    if error.is_connect() || error.is_request() {
        return StudioError::Connection(format!("cannot reach {}: {error}", client.pipeline_url));
    }
    StudioError::QueryExecution(format!("libSQL response ended early: {error}"))
}

fn response_error(status: u16, body: &str) -> StudioError {
    let text = error_message(status, body);
    if is_authentication_error(status, body) {
        StudioError::Authentication(text)
    } else {
        StudioError::QueryExecution(text)
    }
}

/// Post one pipeline body to the client's endpoint and decode the envelope.
/// Cancel releases the request immediately.
async fn pipeline(client: &LibSqlClient, body: Value) -> StudioResult<PipelineResponse> {
    pipeline_at(client, &client.pipeline_url, body).await
}

/// Post one pipeline body to `url` (the client's own endpoint, or the
/// `base_url` a stream was redirected to) and decode the envelope.
async fn pipeline_at(
    client: &LibSqlClient,
    url: &str,
    body: Value,
) -> StudioResult<PipelineResponse> {
    if client.cancel.is_cancelled() {
        return Err(StudioError::QueryCancelled);
    }
    let send = client.request_at(url).json(&body).send();
    let response = tokio::select! {
        biased;
        _ = client.cancel.cancelled() => return Err(StudioError::QueryCancelled),
        response = send => response.map_err(|e| transport_error(client, e))?,
    };
    let status = response.status().as_u16();
    let read = response.bytes();
    let bytes = tokio::select! {
        biased;
        _ = client.cancel.cancelled() => return Err(StudioError::QueryCancelled),
        bytes = read => bytes.map_err(|e| transport_error(client, e))?,
    };
    if status != 200 {
        return Err(response_error(status, &String::from_utf8_lossy(&bytes)));
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        StudioError::QueryExecution(format!("libSQL answered with an unreadable response: {e}"))
    })
}

/// The first request's outcome from a pipeline response.
fn first_result(mut response: PipelineResponse) -> StudioResult<StreamResponse> {
    if response.results.is_empty() {
        return Err(StudioError::QueryExecution(
            "libSQL answered with no results".into(),
        ));
    }
    match response.results.remove(0) {
        StreamResult::Ok { response } => Ok(response),
        StreamResult::Error { error } => Err(StudioError::QueryExecution(error.message)),
    }
}

/// Run one statement and read its whole result (small reads: catalog
/// probes, `list_databases`, the connect probe).
async fn fetch_one(client: &LibSqlClient, sql: &str) -> StudioResult<StatementOutcome> {
    let response = pipeline(client, execute_pipeline(sql.trim())).await?;
    match first_result(response)? {
        StreamResponse::Execute { result } => Ok(decode_statement(result)),
        other => Err(StudioError::QueryExecution(format!(
            "libSQL answered an execute request with {other:?}"
        ))),
    }
}

/// Run `statements` as one conditional batch on one stream.
pub async fn run_batch_on_client(
    client: &LibSqlClient,
    statements: &[String],
) -> StudioResult<BatchOutcome> {
    if statements.is_empty() {
        return Ok(BatchOutcome::default());
    }
    let response = pipeline(client, batch_pipeline(statements)).await?;
    match first_result(response)? {
        StreamResponse::Batch { result } => Ok(decode_batch(result)),
        other => Err(StudioError::QueryExecution(format!(
            "libSQL answered a batch request with {other:?}"
        ))),
    }
}

/// Materialized read of one statement, no safety policy. Statements that
/// return no rows yield an empty result set.
pub async fn query_rows_on_client(
    client: &LibSqlClient,
    sql: &str,
) -> StudioResult<MaterializedResultSet> {
    let outcome = fetch_one(client, sql).await?;
    Ok(MaterializedResultSet {
        columns: outcome.columns,
        rows: outcome.rows,
    })
}

/// The first cell of the first row, as text, or `None` when the statement
/// returned nothing.
pub async fn query_scalar_on_client(
    client: &LibSqlClient,
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

/// Run a raw script with no safety policy and no result decoding; the
/// first failing statement ends the batch with its error.
pub async fn execute_batch_on_client(client: &LibSqlClient, sql: &str) -> StudioResult<()> {
    let outcome = run_batch_on_client(client, &split_script(sql)).await?;
    match outcome.error {
        Some(error) => Err(StudioError::QueryExecution(error.message)),
        None => Ok(()),
    }
}

/// Run a statement, returning its affected-row count (INSERT / UPDATE /
/// DELETE); 0 for DDL and reads.
pub async fn execute_statement_on_client(client: &LibSqlClient, sql: &str) -> StudioResult<u64> {
    Ok(fetch_one(client, sql).await?.affected_rows)
}

/// Materialized read of one parameterized statement on a fresh stream that
/// closes as soon as it has answered.
pub async fn query_rows_with_params_on_client(
    client: &LibSqlClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<MaterializedResultSet> {
    let mut stream = LibSqlStream::open(client.clone());
    let outcome = stream.execute_closing(sql, params).await?;
    Ok(MaterializedResultSet {
        columns: outcome.columns,
        rows: outcome.rows,
    })
}

/// Run one parameterized write on a fresh stream, returning its
/// affected-row count.
pub async fn execute_params_on_client(
    client: &LibSqlClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<u64> {
    let mut stream = LibSqlStream::open(client.clone());
    Ok(stream.execute_closing(sql, params).await?.affected_rows)
}

/// One server-side Hrana stream held open across HTTP requests through its
/// baton, so a transaction begun in one request is still open in the next —
/// the shape row editing and Data Transfer need (`BEGIN`, guarded writes
/// whose affected-row counts decide what happens next, then `COMMIT` or
/// `ROLLBACK`). The server expires an idle stream after a few seconds of
/// silence and rolls its transaction back, so a stream is for one
/// operation, not for keeping between user actions. [`LibSqlStream::close`]
/// releases it; dropping an un-closed stream leaves it to that expiry.
pub struct LibSqlStream {
    client: LibSqlClient,
    baton: Option<String>,
    url: String,
    open: bool,
}

impl LibSqlStream {
    /// A stream that has not spoken to the server yet; the first request
    /// creates it.
    pub fn open(client: LibSqlClient) -> Self {
        let url = client.pipeline_url.clone();
        Self {
            client,
            baton: None,
            url,
            open: true,
        }
    }

    /// Whether the stream is still usable (not closed by [`Self::close`]).
    pub fn is_open(&self) -> bool {
        self.open
    }

    async fn send(&mut self, requests: Vec<Value>, close: bool) -> StudioResult<PipelineResponse> {
        if !self.open {
            return Err(StudioError::QueryExecution(
                "the libSQL stream is closed".into(),
            ));
        }
        let body = pipeline_body(self.baton.as_deref(), requests, close);
        let response = match pipeline_at(&self.client, &self.url, body).await {
            Ok(response) => response,
            Err(error) => {
                // Whatever happened, the stream cannot be trusted any more.
                self.open = false;
                return Err(error);
            }
        };
        if close {
            self.open = false;
            self.baton = None;
        } else {
            self.baton = response.baton.clone();
            if let Some(base) = response.base_url.as_deref().filter(|b| !b.is_empty()) {
                self.url = format!("{}/{PIPELINE_PATH}", base.trim_end_matches('/'));
            }
        }
        Ok(response)
    }

    /// Run one parameterized statement on the stream and keep it open.
    pub async fn execute(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<StatementOutcome> {
        let response = self.send(vec![execute_request(sql, params)], false).await?;
        Self::statement_result(response)
    }

    /// Run one parameterized statement and close the stream in the same
    /// request (a `COMMIT` or `ROLLBACK`, or a one-off read).
    pub async fn execute_closing(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<StatementOutcome> {
        let response = self.send(vec![execute_request(sql, params)], true).await?;
        Self::statement_result(response)
    }

    /// Run `steps` as one conditional batch on the stream (each step runs
    /// only if the previous one succeeded) and keep the stream open, so the
    /// caller can inspect the outcomes before deciding to commit.
    pub async fn batch(&mut self, steps: &[(String, Vec<SqlParam>)]) -> StudioResult<BatchOutcome> {
        if steps.is_empty() {
            return Ok(BatchOutcome::default());
        }
        let response = self.send(vec![batch_request(steps)], false).await?;
        match first_result(response)? {
            StreamResponse::Batch { result } => Ok(decode_batch(result)),
            other => Err(StudioError::QueryExecution(format!(
                "libSQL answered a batch request with {other:?}"
            ))),
        }
    }

    /// Release the stream (the server rolls back anything still open).
    pub async fn close(&mut self) -> StudioResult<()> {
        if !self.open {
            return Ok(());
        }
        if self.baton.is_none() {
            // Nothing was ever created server-side.
            self.open = false;
            return Ok(());
        }
        self.send(Vec::new(), true).await.map(|_| ())
    }

    fn statement_result(response: PipelineResponse) -> StudioResult<StatementOutcome> {
        match first_result(response)? {
            StreamResponse::Execute { result } => Ok(decode_statement(result)),
            other => Err(StudioError::QueryExecution(format!(
                "libSQL answered an execute request with {other:?}"
            ))),
        }
    }
}

/// Run a script on an open client, streaming results through the shared
/// driver contract (events on the bus, rows on the bounded channel).
/// Applies the profile's safety policy; the rollback wrap works because the
/// whole script runs on one server-side stream.
pub async fn execute_on_client(
    client: &LibSqlClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    let sql = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    let statements = split_script(&sql);
    let batch = run_batch_on_client(client, &statements).await?;
    for outcome in batch.outcomes {
        if client.cancel.is_cancelled() {
            return Err(StudioError::QueryCancelled);
        }
        if outcome.has_result_set() {
            streamer.begin_result_set(outcome.columns).await?;
            for row in outcome.rows {
                if !streamer.push_row(row).await? {
                    // The row consumer went away: stop without the rest.
                    return streamer.finish().await;
                }
            }
        } else {
            streamer.statement_completed(outcome.affected_rows)?;
        }
    }
    if let Some(error) = batch.error {
        return Err(StudioError::QueryExecution(error.message));
    }
    streamer.finish().await
}

/// `SqlTransport` implementation for libSQL / Turso, usable with the shared
/// [`ConnectionService`].
#[derive(Debug, Clone, Default)]
pub struct LibSqlTransport;

#[async_trait::async_trait]
impl SqlTransport for LibSqlTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    /// A libSQL endpoint serves one database; SQLite names it `main`.
    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        let set =
            query_rows_on_client(&client, "SELECT name FROM main.pragma_database_list").await?;
        let mut databases: Vec<DatabaseSummary> = set
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
            .collect();
        if databases.is_empty() {
            databases.push(DatabaseSummary {
                name: "main".to_string(),
                state: "ONLINE".to_string(),
                compatibility_level: None,
            });
        }
        Ok(databases)
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

/// Build a libSQL profile from a URL of the form
/// `libsql://host?authToken=…`, `https://host:port?authToken=…` or
/// `http://host:8080` — the shape the `SQLLY_LIBSQL_URL` live-test variable
/// uses. `libsql` and `https` turn encryption on (verifying the certificate
/// unless the URL adds `?trust=1` for a lab server); `http` is plaintext.
pub fn profile_from_url(name: &str, url: &str) -> StudioResult<ConnectionProfile> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|e| StudioError::InvalidProfile(format!("invalid libSQL URL: {e}")))?;
    let https = match parsed.scheme() {
        "http" => false,
        "https" | "libsql" => true,
        other => {
            return Err(StudioError::InvalidProfile(format!(
                "unsupported libSQL URL scheme '{other}'"
            )));
        }
    };
    let host = parsed.host_str().ok_or_else(|| {
        StudioError::InvalidProfileMessage(sqlly_core::DisplayMessage::new(
            "services.libsql.errors.no_host",
            "libSQL URL has no host",
        ))
    })?;
    let mut token = parsed
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case("authToken") || k.eq_ignore_ascii_case("auth_token"))
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    if token.is_empty()
        && let Some(password) = parsed.password()
    {
        token = percent_decode(password);
    }
    let trust = parsed
        .query_pairs()
        .any(|(k, v)| k.eq_ignore_ascii_case("trust") && matches!(&*v, "1" | "true"));
    let port = parsed
        .port()
        .unwrap_or(if https { TLS_PORT } else { DEFAULT_PORT });
    let mut profile = ConnectionProfile::auth_token(
        name,
        host,
        port,
        SecretRef {
            provider: SecretProvider::InMemory,
            key: token,
        },
    );
    profile.encrypt = https;
    profile.trust_server_certificate = https && trust;
    profile.tls_mode = match (https, trust) {
        (false, _) => TlsMode::Disable,
        (true, true) => TlsMode::Require,
        (true, false) => TlsMode::VerifyFull,
    };
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

/// The live-test profile from `SQLLY_LIBSQL_URL`, or `None` so tests
/// self-skip on a checkout without a server.
pub fn live_profile_from_env(name: &str) -> Option<ConnectionProfile> {
    let url = std::env::var("SQLLY_LIBSQL_URL").ok()?;
    match profile_from_url(name, &url) {
        Ok(profile) => Some(profile),
        Err(error) => {
            eprintln!("SQLLY_LIBSQL_URL is not usable: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;
    use std::time::Duration;

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::auth_token(
            "libsql",
            "127.0.0.1",
            8080,
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "tok".into(),
            },
        );
        p.encrypt = false;
        p.tls_mode = TlsMode::Disable;
        p
    }

    #[test]
    fn pipeline_url_follows_tls_mode_turso_hosts_and_brackets_ipv6() {
        let p = profile();
        assert_eq!(
            pipeline_url_for(&p, "127.0.0.1", 8080),
            "http://127.0.0.1:8080/v2/pipeline"
        );
        assert_eq!(
            pipeline_url_for(&p, "::1", 0),
            "http://[::1]:8080/v2/pipeline"
        );
        // Turso is always HTTPS, whatever the profile says.
        assert_eq!(
            pipeline_url_for(&p, "shop-acme.turso.io", 0),
            "https://shop-acme.turso.io:443/v2/pipeline"
        );
        let mut tls = profile();
        tls.tls_mode = TlsMode::VerifyFull;
        assert_eq!(
            pipeline_url_for(&tls, "sqld.example.com", 0),
            "https://sqld.example.com:443/v2/pipeline"
        );
        assert_eq!(
            pipeline_url_for(&tls, "[::1]", 8443),
            "https://[::1]:8443/v2/pipeline"
        );
        let mut legacy = profile();
        legacy.tls_mode = TlsMode::Legacy;
        legacy.encrypt = true;
        assert_eq!(
            pipeline_url_for(&legacy, "h", 0),
            "https://h:443/v2/pipeline"
        );
        assert!(is_turso_host("Shop-Acme.TURSO.IO"));
        assert!(!is_turso_host("turso.io.example.com"));
    }

    #[test]
    fn bearer_token_resolves_auth_token_and_treats_blank_as_anonymous() {
        assert_eq!(bearer_token(&profile()).unwrap().as_deref(), Some("tok"));
        let mut anonymous = profile();
        anonymous.authentication = AuthenticationType::AuthToken {
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: "   ".into(),
            },
        };
        assert_eq!(bearer_token(&anonymous).unwrap(), None);
        // A SQL-login password doubles as the token (CLI --password).
        let mut login = profile();
        login.authentication = AuthenticationType::SqlLogin {
            username: String::new(),
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: "jwt".into(),
            },
        };
        assert_eq!(bearer_token(&login).unwrap().as_deref(), Some("jwt"));
        let mut entra = profile();
        entra.authentication = AuthenticationType::EntraBrowser;
        assert!(matches!(
            bearer_token(&entra),
            Err(StudioError::InvalidProfile(_))
        ));
        let mut integrated = profile();
        integrated.authentication = AuthenticationType::Integrated;
        assert!(matches!(
            bearer_token(&integrated),
            Err(StudioError::NotImplemented(_))
        ));
        let mut iam = profile();
        iam.authentication = AuthenticationType::GcpCloudSqlIam {
            username: "x".into(),
        };
        assert!(matches!(
            bearer_token(&iam),
            Err(StudioError::InvalidProfile(_))
        ));
    }

    #[test]
    fn profile_from_url_fills_every_field() {
        let p = profile_from_url("live", "http://127.0.0.1:8080").unwrap();
        assert_eq!(p.database_type, DatabaseType::LibSql);
        assert_eq!(p.server, "127.0.0.1");
        assert_eq!(p.port, 8080);
        assert!(!p.encrypt);
        assert_eq!(p.tls_mode, TlsMode::Disable);
        assert!(matches!(
            &p.authentication,
            AuthenticationType::AuthToken { secret_ref } if secret_ref.key.is_empty()
        ));

        let turso = profile_from_url("t", "libsql://shop-acme.turso.io?authToken=eyJ.a.b").unwrap();
        assert_eq!(turso.server, "shop-acme.turso.io");
        assert_eq!(turso.port, 443);
        assert!(turso.encrypt && !turso.trust_server_certificate);
        assert_eq!(turso.tls_mode, TlsMode::VerifyFull);
        assert!(matches!(
            &turso.authentication,
            AuthenticationType::AuthToken { secret_ref } if secret_ref.key == "eyJ.a.b"
        ));

        let lab = profile_from_url("l", "https://:s%40cret@sqld.lab:8443?trust=1").unwrap();
        assert_eq!(lab.port, 8443);
        assert!(lab.trust_server_certificate);
        assert_eq!(lab.tls_mode, TlsMode::Require);
        assert!(matches!(
            &lab.authentication,
            AuthenticationType::AuthToken { secret_ref } if secret_ref.key == "s@cret"
        ));
        assert!(profile_from_url("x", "mysql://h").is_err());
        assert!(profile_from_url("x", "not a url").is_err());
    }

    #[test]
    fn response_errors_classify_authentication() {
        assert!(matches!(
            response_error(401, r#"{"error":"Unauthorized: empty JWT token"}"#),
            StudioError::Authentication(text) if text.contains("Unauthorized")
        ));
        assert!(matches!(
            response_error(400, r#"{"error":"JWT error: InvalidToken"}"#),
            StudioError::Authentication(_)
        ));
        assert!(matches!(
            response_error(400, r#"{"message":"bad request","code":"X"}"#),
            StudioError::QueryExecution(text) if text == "bad request"
        ));
    }

    /// Live: connect, list the database, stream 20k rows in batches, map
    /// every type, count writes in a multi-statement script, stop at the
    /// first failing statement with the server's message, and cancel.
    /// Opt-in through `SQLLY_LIBSQL_URL` (`http://127.0.0.1:8080` for the
    /// `ghcr.io/tursodatabase/libsql-server` container).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_streams_types_counts_and_cancels() {
        let Some(profile) = live_profile_from_env("libsql-live") else {
            eprintln!("skipping: SQLLY_LIBSQL_URL not set");
            return;
        };
        let transport = LibSqlTransport;
        transport.test_connection(&profile).await.unwrap();
        let dbs = transport.list_databases(&profile).await.unwrap();
        assert_eq!(dbs[0].name, "main", "{dbs:?}");

        let client = open_client(&profile).await.unwrap();
        assert!(
            query_scalar_on_client(&client, "SELECT sqlite_version()")
                .await
                .unwrap()
                .unwrap()
                .starts_with('3')
        );

        // 20k rows arrive through the bounded channel in several batches.
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
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 20000) SELECT x AS n FROM c",
            &events,
            Some(&tx),
        )
        .await
        .unwrap();
        drop(tx);
        let (total, batches) = drain.await.unwrap();
        assert_eq!(total, 20_000);
        assert!(
            batches > 10,
            "rows must arrive in many batches, got {batches}"
        );
        let mut saw_columns = false;
        while let Ok(event) = sub.try_recv() {
            if let sqlly_core::EventPayload::ResultSetStarted { columns, .. } = event.payload {
                assert_eq!(columns[0].name, "n");
                saw_columns = true;
            }
        }
        assert!(saw_columns);

        // Every SQLite storage class maps into the grid, with decltypes.
        let table = format!("sqlly_live_{}", uuid::Uuid::new_v4().simple());
        execute_batch_on_client(
            &client,
            &format!(
                "CREATE TABLE {table} (id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB, untyped);\n\
                 INSERT INTO {table} (name, score, data, untyped) VALUES ('é', 1.5, X'01ff', NULL)"
            ),
        )
        .await
        .unwrap();
        let set = query_rows_on_client(
            &client,
            &format!(
                "SELECT id, name, score, data, untyped, 9223372036854775807 AS big, -1 AS neg, 1e400 AS inf FROM {table}"
            ),
        )
        .await
        .unwrap();
        let row = &set.rows[0];
        let by_name = |name: &str| {
            let idx = set.columns.iter().position(|c| c.name == name).unwrap();
            (row[idx].clone(), set.columns[idx].data_type.clone())
        };
        assert_eq!(by_name("id"), (CellValue::I64(1), "integer".into()));
        assert_eq!(
            by_name("name"),
            (CellValue::String("é".into()), "text".into())
        );
        assert_eq!(by_name("score"), (CellValue::F64(1.5), "real".into()));
        assert_eq!(
            by_name("data"),
            (CellValue::Bytes(vec![0x01, 0xff]), "blob".into())
        );
        assert_eq!(by_name("untyped"), (CellValue::Null, "".into()));
        assert_eq!(by_name("big").0, CellValue::I64(i64::MAX));
        assert_eq!(by_name("neg").0, CellValue::I64(-1));
        assert_eq!(by_name("inf").0, CellValue::String("non-finite".into()));

        // A script mixes counts and result sets in order and stops at the
        // first failure with the server's own message; the transaction it
        // was in rolls back.
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let err = execute_on_client(
            &client,
            &profile,
            &format!(
                "BEGIN;\n\
                 INSERT INTO {table} (name) VALUES ('a'), ('b'), ('c');\n\
                 SELECT count(*) AS n FROM {table};\n\
                 SELECT nope FROM {table};\n\
                 INSERT INTO {table} (name) VALUES ('never');\n\
                 COMMIT"
            ),
            &events,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no such column: nope"), "{err}");
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
        assert_eq!(counts, vec![0, 3], "{counts:?}");
        assert_eq!(rows, vec![vec![CellValue::I64(4)]]);
        let remaining = query_scalar_on_client(&client, &format!("SELECT count(*) FROM {table}"))
            .await
            .unwrap();
        assert_eq!(
            remaining.as_deref(),
            Some("1"),
            "the failed script rolled back"
        );

        // A parse error names the server's message too.
        let bus = EventBus::new(8);
        let _keep = bus.subscribe();
        let err = execute_on_client(&client, &profile, "SELEC 1", &bus, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("syntax error"), "{err}");

        // Cancel releases the in-flight request within a moment.
        let slow = open_client(&profile).await.unwrap();
        let handle = cancel_handle(&slow);
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            handle.cancel();
        });
        let result = execute_on_client(
            &slow,
            &profile,
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 40000000) SELECT count(*) FROM c",
            &bus,
            None,
        )
        .await;
        assert!(
            matches!(result, Err(StudioError::QueryCancelled)),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));

        execute_batch_on_client(&client, &format!("DROP TABLE {table}"))
            .await
            .unwrap();
    }
}
