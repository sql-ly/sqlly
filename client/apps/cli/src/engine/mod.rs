use anyhow::{Context, Result};

use crate::engine_error::EngineError;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use sqlly_ai::catalog::{
    BuildReport, CatalogObject, EmbeddingBuildReport, LlmResponse, SchemaPurpose, SchemaSearchHit,
    SyncReport,
};
use sqlly_core::{
    CellValue, ConnectionProfile, Event, EventBus, RowReceiver, SecretProvider, SecretRef,
    StudioResult,
};
use sqlly_intel::modelengine::validate::{
    EngineMode, ExemptionSet, IssueClass, QueryValidationRequest, Severity, ValidationScope,
    validate_query,
};
use sqlly_lang::sqlmodel::SqlName;
use std::collections::HashMap;
use std::fs::OpenOptions;
#[cfg(test)]
use std::io::ErrorKind;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(test)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

// The wire codec (frame tags, version constants, size limits, and the
// little-endian binary cursor) now lives in the shared `sqlly-engine-protocol`
// crate so the CLI, GPUI remote sessions, the bridge, and tests all speak one
// codec. This module keeps engine request *dispatch*, database access,
// `EngineLog` logging, and subprocess behavior. `BinaryReader` is imported
// under its historical local name `BinaryFrameReader` to keep decoders stable.
use sqlly_services::DefaultCatalogService;

use sqlly_remote::engine_protocol::{
    BINARY_REQUEST_LAYOUT_VERSION, BinaryReader as BinaryFrameReader,
    BinaryWriter as BinaryBodyWriter, FRAME_TAG_AI_CACHE_BUILD_REPORT,
    FRAME_TAG_CATALOG_MAINTENANCE_REPORT, FRAME_TAG_CATALOG_SYNC_REPORT,
    FRAME_TAG_EMBEDDINGS_BUILD_REPORT, FRAME_TAG_EVENT, FRAME_TAG_INVALIDATE_AI_CACHE_REPORT,
    FRAME_TAG_JOIN_PATH_REPORT, FRAME_TAG_PONG, FRAME_TAG_PROVIDER_STATUS_REPORT,
    FRAME_TAG_QUERY_SUMMARY_REPORT, FRAME_TAG_READY, FRAME_TAG_REQUEST_CANCELLED,
    FRAME_TAG_REQUEST_COMPLETED, FRAME_TAG_REQUEST_FAILED, FRAME_TAG_REQUEST_STARTED,
    FRAME_TAG_ROW_EDIT_REPORT, FRAME_TAG_ROWS_BATCH, FRAME_TAG_SCHEMA_ANSWER,
    FRAME_TAG_SCHEMA_SEARCH_RESULTS, FRAME_TAG_SHUTDOWN_ACK, FRAME_TAG_SQL_GENERATION_REPORT,
    FRAME_TAG_SQL_REVIEW_REPORT, FRAME_TAG_SQL_VALIDATION_REPORT, FRAME_TAG_VALIDATION,
    MAX_FRAME_BYTES, PROTOCOL_VERSION, REQUEST_TAG_ASK_SCHEMA, REQUEST_TAG_BUILD_AI_CACHE,
    REQUEST_TAG_BUILD_EMBEDDINGS, REQUEST_TAG_CANCEL, REQUEST_TAG_CATALOG_MAINTENANCE,
    REQUEST_TAG_EXECUTE_QUERY, REQUEST_TAG_FIND_JOIN_PATH, REQUEST_TAG_GENERATE_SQL,
    REQUEST_TAG_INVALIDATE_AI_CACHE, REQUEST_TAG_PING, REQUEST_TAG_PROVIDER_STATUS,
    REQUEST_TAG_REVIEW_SQL, REQUEST_TAG_ROW_EDIT, REQUEST_TAG_SEARCH_SCHEMA, REQUEST_TAG_SHUTDOWN,
    REQUEST_TAG_SUMMARIZE_QUERY, REQUEST_TAG_SYNC_CATALOG, REQUEST_TAG_VALIDATE,
    REQUEST_TAG_VALIDATE_SQL,
};

/// Capabilities this engine advertises in its `Ready` frame (see
/// `sqlly_remote::engine_protocol::capabilities`). A relay client checks these
/// before sending anything an older bridge-side engine would reject. Add a
/// name here in the same change that makes the engine accept the feature.
pub(crate) const ENGINE_CAPABILITIES: &[&str] = &[
    sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS,
    sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_FK_LABELS,
    sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_APPLY_BATCH,
    sqlly_remote::engine_protocol::CAPABILITY_QUERY_CELL_PREVIEWS,
];

mod decode;
mod encode;
mod handlers;
mod io;
mod log;
mod state;

#[cfg(test)]
mod tests;

pub(crate) use self::decode::*;
pub(crate) use self::encode::*;
pub(crate) use self::handlers::*;
pub(crate) use self::io::*;
pub(crate) use self::log::*;
pub(crate) use self::state::*;

pub async fn run(proxy_credentials: Option<&Path>) -> Result<()> {
    // Proxy mode: load the credential file before touching stdin so a bad
    // file is a clear startup error on stderr, never a stream of
    // `request_failed`s the browser cannot interpret.
    let proxy = match proxy_credentials {
        Some(path) => {
            // A proxied engine serves browsers that must never reach this
            // host's filesystem or network through a file engine: DuckDB may
            // neither download extensions nor touch external state (file
            // scans, COPY, ATTACH, httpfs). Both settings apply to every
            // instance opened from now on.
            sqlly_engines::duckdb::set_extension_downloads_allowed(false);
            sqlly_engines::duckdb::set_external_access_allowed(false);
            Some(Arc::new(load_proxy_catalog(path)?))
        }
        None => None,
    };
    // Read stdin on a dedicated OS thread (synchronous `std::io`) and forward
    // raw request frames over a channel, rather than using `tokio::io::stdin()`.
    // This keeps stdin entirely off the async runtime (tokio's stdin is backed
    // by the blocking pool and has cancellation quirks for a long-lived
    // protocol process).
    let (req_tx, req_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::Builder::new()
        .name("sqlly-engine-stdin".to_string())
        .spawn(move || stdin_reader(req_tx))
        .context("spawn engine stdin reader")?;
    run_with_requests(req_rx, proxy).await
}

/// Parse a credential file into the catalog of connections a proxy-mode
/// engine may open. Secrets stay in memory; the file is never rewritten.
pub(crate) fn load_proxy_catalog(
    path: &Path,
) -> Result<sqlly_core::credential_proxy::ProxyCatalog> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read proxy credential file {}", path.display()))?;
    sqlly_core::credential_proxy::ProxyCatalog::from_credential_text(&text)
        .map_err(|e| anyhow::anyhow!("proxy credential file {}: {e}", path.display()))
}

/// Engine event loop driven by request frames arriving on a channel. Shared by
/// the production stdin-thread path (`run`) so the stdout writer/watchdog setup
/// lives in one place.
async fn run_with_requests(
    mut requests: mpsc::Receiver<Vec<u8>>,
    proxy: Option<Arc<sqlly_core::credential_proxy::ProxyCatalog>>,
) -> Result<()> {
    let (frames_tx, frames_rx) = mpsc::channel::<EngineFrame>(1024);
    // The engine protocol is binary length-prefixed framing over stdout. We
    // deliberately use a dedicated blocking thread writing to the synchronous
    // `std::io::stdout()` rather than `tokio::io::stdout()`: the async stdout
    // wrapper buffers writes and does not reliably push isolated/trailing
    // frames (e.g. a terminal `request_failed`/`request_completed` arriving
    // after a time gap) out to the OS pipe even when `flush().await` returns
    // Ok. That left clients (Swift) hung on "Query Running" forever. Writing
    // synchronously and flushing on the blocking pool guarantees each frame
    // reaches the pipe immediately.
    let mut writer = tokio::task::spawn_blocking(move || write_frames_blocking(frames_rx));
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::with_proxy(proxy);
    let (catalog, _diagnostics) = DefaultCatalogService::from_env().await;

    // Wrap the raw frame channel so every outbound frame is logged, correlated
    // to the request id it answers, before it reaches the writer. The
    // `EngineLog` (and thus all logging) is gated by the `ENGINE_LOG_ENABLED`
    // compile constant via `EngineLog::resolve`.
    let log = EngineLog::resolve();
    let frames = LoggedSender::new(frames_tx, log.clone());

    frames
        .send(EngineFrame::Ready {
            protocol_version: PROTOCOL_VERSION,
        })
        .await;

    loop {
        tokio::select! {
            frame = requests.recv() => {
                match frame {
                    Some(frame) => {
                        let dispatch_result = AssertUnwindSafe(
                            dispatch_request(&frame, &frames, &active, &pool, &catalog)
                        )
                        .catch_unwind()
                        .await;
                        match dispatch_result {
                            Ok(continue_loop) => {
                                if !continue_loop {
                                    break;
                                }
                            }
                            Err(panic) => {
                                let msg = extract_panic_message(&*panic);
                                eprintln!("[engine] dispatch_request panicked: {msg}");
                                let id = best_effort_request_id(&frame)
                                    .unwrap_or_else(|| "unknown".to_string());
                                frames
                                    .send(EngineFrame::RequestFailed {
                                        id,
                                        error: EngineError::internal(format!("engine dispatch panicked: {msg}")),
                                    })
                                    .await;
                            }
                        }
                    }
                    None => break,
                }
            }
            result = &mut writer => {
                match result {
                    Ok(Ok(())) => {
                        eprintln!("[engine] stdout writer exited; stopping request loop");
                    }
                    Ok(Err(ref e)) => {
                        eprintln!("[engine] stdout writer error: {e}; stopping request loop");
                    }
                    Err(ref e) => {
                        eprintln!("[engine] stdout writer task panicked: {e}; stopping request loop");
                    }
                }
                break;
            }
        }
    }

    for (_id, handle) in active.lock().await.drain() {
        handle.abort();
    }

    drop(frames);
    writer.await.context("engine writer task panicked")??;

    // Reaching here means the request loop exited normally — either an explicit
    // `Shutdown` request or stdin EOF (the parent app closing the engine
    // gracefully). A crash (panic/abort/SIGSEGV/SIGKILL) never gets here, so
    // trimming the log to its last lines on this path leaves crashed sessions'
    // logs fully intact for debugging while keeping clean sessions bounded. The
    // writer has already finished, so every frame from this session is on disk
    // before we trim.
    if let Some(log) = &log {
        log.truncate_to_last_lines(ENGINE_LOG_TAIL_LINES);
    }
    Ok(())
}
