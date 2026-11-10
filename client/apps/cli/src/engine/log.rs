use super::*;

use std::io::Write as _;

// ---------------------------------------------------------------------------
// Engine request/response correlation log
// ---------------------------------------------------------------------------
//
// Every call into the Rust engine (`dispatch_request`) is logged with a
// timestamp, the request's message id, and its type/body the moment it is
// received. Every frame the engine sends back (`LoggedSender::send`) is logged
// with a timestamp, the id of the request that triggered it, the frame type,
// and — critically — the error text for `request_failed` frames. Because each
// request id appears on its REQ line and again on every correlated RES line, a
// request that crashes, wedges, or otherwise produces no terminal frame leaves
// a REQ line whose id never reaches a terminal RES line. That dangling id is
// the smoking gun for "what caused this message to fail".
//
// Whether logging is compiled in at all is governed by [`ENGINE_LOG_ENABLED`],
// a compile-time constant. When it is `false` the compiler eliminates the
// logging path entirely (no file is ever opened, no formatting cost is paid).
// It is currently ON.

/// Compile-time master switch for engine request/response logging. When
/// `false`, all logging is dead-code-eliminated and the engine pays zero
/// runtime cost. Currently ON.
pub(crate) const ENGINE_LOG_ENABLED: bool = true;

/// Environment variable that overrides the engine log file path. When unset (or
/// empty) the default path [`default_engine_log_path`] is used.
pub(crate) const ENGINE_LOG_ENV: &str = "SQLLY_ENGINE_LOG";

/// Default location for the engine correlation log under the user's data
/// directory (not the shared temp dir). See `AGENTS.md` for per-OS paths.
pub(crate) fn default_engine_log_path() -> PathBuf {
    #[cfg(target_arch = "wasm32")]
    {
        // Browser builds have no home directory; keep the historical temp path.
        return std::env::temp_dir().join("sqlly-engine.log");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|home| home.join(".local").join("share"))
                    .unwrap_or_else(std::env::temp_dir)
            });
        return base.join("sqlly").join("sqlly-engine.log");
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let base = dirs::data_dir()
            .or({
                #[cfg(target_os = "windows")]
                {
                    std::env::var_os("APPDATA").map(PathBuf::from)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    None
                }
            })
            .unwrap_or_else(std::env::temp_dir);
        base.join("SQLLY").join("sqlly-engine.log")
    }
}

/// Create the log's parent directory with owner-only permissions on Unix.
pub(crate) fn ensure_engine_log_parent(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    if parent.as_os_str().is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(parent);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
}

/// `OpenOptions` that create the correlation log owner-read/write-only on Unix.
pub(crate) fn owner_only_open_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Tighten an existing log file to owner-only on Unix (best-effort).
pub(crate) fn tighten_engine_log_file_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                let _ =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077));
            }
        }
    }
    let _ = path;
}

/// Number of trailing log lines retained when the engine shuts down gracefully.
/// On a clean (non-crash) exit the log is rewritten down to just its last
/// [`ENGINE_LOG_TAIL_LINES`] lines so it never grows without bound across
/// sessions. A crash deliberately leaves the file untouched so the dangling
/// REQ line that caused it is preserved for post-mortem analysis.
pub(crate) const ENGINE_LOG_TAIL_LINES: usize = 50;

/// Milliseconds since the Unix epoch — the wall-clock timestamp stamped onto
/// every REQ and RES line so request/response latency is recoverable by id.
pub(crate) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Collapse newlines/carriage returns so every logged record stays on exactly
/// one line, keeping the "REQ id with no terminal RES" scan trivially
/// grep-able.
pub(crate) fn single_line(s: &str) -> String {
    s.replace('\n', "\\n").replace('\r', "\\r")
}

/// Append-only correlation logger bound to one file path. Cloneable (cheap:
/// just an `Arc<PathBuf>`) so it can be threaded into every per-query task that
/// sends frames. Each write opens, appends, flushes, and closes the file so a
/// partial write survives a hard crash and so the line is on disk before the
/// work that might crash even starts.
#[derive(Clone)]
pub(crate) struct EngineLog {
    path: Arc<PathBuf>,
}

impl EngineLog {
    /// Resolve the logger, honoring the [`ENGINE_LOG_ENABLED`] compile switch.
    /// Returns `None` when logging is compiled out, so callers can skip all
    /// work behind a single `if let Some(log)`.
    pub(crate) fn resolve() -> Option<EngineLog> {
        if !ENGINE_LOG_ENABLED {
            return None;
        }
        let path = match std::env::var_os(ENGINE_LOG_ENV) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => default_engine_log_path(),
        };
        Some(EngineLog {
            path: Arc::new(path),
        })
    }

    /// For tests: build a logger pointed at an explicit path, bypassing env
    /// resolution (but still respecting the compile switch via `resolve` in
    /// production).
    #[cfg(test)]
    pub(crate) fn at(path: impl Into<PathBuf>) -> EngineLog {
        EngineLog {
            path: Arc::new(path.into()),
        }
    }

    /// Append one line to the log. When `durable` is set the line is fsync'd to
    /// the physical device before returning, so it survives even an immediate
    /// hard crash (panic, abort, SIGSEGV) of this process — this is what makes
    /// the request line that *caused* a crash recoverable when no response line
    /// is ever written. Each call opens, appends, and (durably) flushes, so
    /// concurrent one-shot work interleaves at line boundaries and a partial
    /// write can never corrupt earlier lines.
    pub(crate) fn append_line(&self, line: &str, durable: bool) {
        // Best-effort diagnostics: never fail a request because the log file
        // could not be written.
        ensure_engine_log_parent(self.path.as_path());
        if let Ok(mut file) = owner_only_open_options()
            .create(true)
            .append(true)
            .open(self.path.as_path())
        {
            tighten_engine_log_file_permissions(self.path.as_path());
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
            if durable {
                // fsync: push the bytes past the OS page cache to the device so
                // the culprit request is on stable storage before we run the
                // work that might crash the whole process.
                let _ = file.sync_all();
            }
        }
    }

    /// Record an inbound request the moment it is received, BEFORE any parsing
    /// or work, and fsync it, so a crash leaves this line durably on disk with
    /// no terminal RES line sharing its id. That dangling REQ id is the exact
    /// input that crashed the engine. Credential values (passwords, access
    /// tokens, secret refs) are scrubbed before the line is written — the id,
    /// type, SQL text, and every non-secret field keep their diagnostic value.
    pub(crate) fn log_request(&self, id: &str, kind: &str, raw: &str) {
        self.append_line(
            &format!(
                "{ts} REQ id={id} type={kind} {raw}",
                ts = now_millis(),
                raw = single_line(&sqlly_core::operational::redact_secrets(raw)),
            ),
            true,
        );
    }

    /// Record an outbound frame, correlated to the request id it answers. These
    /// are flushed but not fsync'd: a response line proves the request did NOT
    /// crash, so losing the very last one to a power cut is acceptable and keeps
    /// the high-volume streaming path (one line per row batch) cheap.
    pub(crate) fn log_response(&self, id: &str, kind: &str, detail: &str) {
        if detail.is_empty() {
            self.append_line(
                &format!("{ts} RES id={id} type={kind}", ts = now_millis()),
                false,
            );
        } else {
            self.append_line(
                &format!(
                    "{ts} RES id={id} type={kind} {detail}",
                    ts = now_millis(),
                    detail = single_line(&sqlly_core::operational::redact_secrets(detail)),
                ),
                false,
            );
        }
    }

    /// Rewrite the log down to its last `keep` lines. Called ONLY on a graceful
    /// engine shutdown (the normal exit of the request loop), never on a crash,
    /// so a session that ends cleanly trims the file while a session that
    /// crashed leaves the full log — including the dangling REQ line that
    /// caused the crash — intact for debugging.
    ///
    /// The rewrite is atomic: the retained tail is written to a sibling temp
    /// file which is then `rename`d over the original. A crash mid-truncate
    /// therefore leaves either the complete old log or the complete new tail,
    /// never a half-written file. Best-effort throughout — a failure to trim
    /// must never turn a clean shutdown into an error.
    pub(crate) fn truncate_to_last_lines(&self, keep: usize) {
        let path = self.path.as_path();
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // No log file (logging never wrote anything) or unreadable: nothing
            // to trim.
            Err(_) => return,
        };

        let lines: Vec<&str> = contents.lines().collect();
        if lines.len() <= keep {
            // Already at or under the cap; avoid a pointless rewrite.
            return;
        }

        let tail = lines[lines.len() - keep..].join("\n");
        // Preserve the trailing newline so the file stays append-clean for the
        // next session (every `append_line` writes a full line + newline).
        let mut tail_with_newline = tail;
        tail_with_newline.push('\n');

        let tmp_path = path.with_extension("log.trim");
        let staged = (|| -> std::io::Result<()> {
            ensure_engine_log_parent(&tmp_path);
            let mut file = owner_only_open_options()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            file.write_all(tail_with_newline.as_bytes())?;
            file.flush()?;
            tighten_engine_log_file_permissions(&tmp_path);
            Ok(())
        })();
        if staged.is_err() {
            // Could not stage the trimmed file; leave the original untouched.
            let _ = std::fs::remove_file(&tmp_path);
            return;
        }
        // Atomic replace. If rename fails, clean up the temp file and leave the
        // (untrimmed but valid) original in place.
        if std::fs::rename(&tmp_path, path).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

/// A [`FrameSender`] paired with an optional [`EngineLog`]. Sending a frame
/// through this wrapper logs it (correlated to the triggering request id)
/// immediately before it is handed to the writer, so the log records frames in
/// the same order the client will see them. Cloning is cheap and is what every
/// per-query task receives, so all responses — including ones emitted from
/// spawned tasks long after the request returned — are correlated.
#[derive(Clone)]
pub(crate) struct LoggedSender {
    pub(crate) inner: FrameSender,
    pub(crate) log: Option<EngineLog>,
}

impl LoggedSender {
    pub(crate) fn new(inner: FrameSender, log: Option<EngineLog>) -> Self {
        Self { inner, log }
    }

    pub(crate) async fn send(&self, frame: EngineFrame) {
        if let Some(log) = &self.log {
            let (id, kind, detail) = frame_log_fields(&frame);
            log.log_response(id, kind, &detail);
        }
        if let Err(err) = self.inner.send(frame).await {
            eprintln!("[engine] frame channel closed, response dropped: {err}");
        }
    }
}

/// Extract `(triggering request id, frame type, debug detail)` from a frame for
/// the correlation log. The id field of every frame IS the id of the request
/// that triggered it. `detail` carries the failure reason for `request_failed`
/// (the key signal for debugging a failing message) and lightweight shape info
/// for streaming frames, and is empty otherwise.
pub(crate) fn frame_log_fields(frame: &EngineFrame) -> (&str, &'static str, String) {
    match frame {
        EngineFrame::Ready { .. } => ("-", "ready", String::new()),
        EngineFrame::Pong { id } => (id, "pong", String::new()),
        EngineFrame::RequestStarted { id } => (id, "request_started", String::new()),
        EngineFrame::Event { id, event } => (
            id,
            "event",
            serde_json::to_string(event).unwrap_or_default(),
        ),
        EngineFrame::RowsBatch {
            id,
            result_set_index,
            rows,
        } => (
            id,
            "rows_batch",
            format!("result_set={result_set_index} rows={}", rows.len()),
        ),
        EngineFrame::TruncatedCells {
            id,
            result_set_index,
            cells,
        } => (
            id,
            "truncated_cells",
            format!("result_set={result_set_index} cells={}", cells.len()),
        ),
        EngineFrame::RequestCompleted { id } => (id, "request_completed", String::new()),
        EngineFrame::RequestFailed { id, error } => {
            (id, "request_failed", format!("error={error}"))
        }
        EngineFrame::RequestCancelled { id, target_id } => {
            (id, "request_cancelled", format!("target_id={target_id}"))
        }
        EngineFrame::ShutdownAck { id } => (id, "shutdown_ack", String::new()),
        EngineFrame::ValidationReport {
            id, diagnostics, ..
        } => (
            id,
            "validation_report",
            format!("diagnostics={}", diagnostics.len()),
        ),
        EngineFrame::RowEditReport { id, .. } => (id, "row_edit_report", String::new()),
        EngineFrame::CatalogSyncReport { id, report } => (
            id,
            "catalog_sync_report",
            format!(
                "added={} changed={} removed={} ai_invalidated={}",
                report.added, report.changed, report.removed, report.ai_cache_invalidated
            ),
        ),
        EngineFrame::ProviderStatusReport { id, report } => (
            id,
            "provider_status_report",
            format!(
                "llm={} embedding={} reachable={:?}",
                report.llm_active, report.embedding_active, report.runtime_reachable
            ),
        ),
        EngineFrame::SqlReviewReport { id, report } => (
            id,
            "sql_review_report",
            format!("issues={}", report.validation.issues.len()),
        ),
        EngineFrame::QuerySummaryReport { id, .. } => (id, "query_summary_report", String::new()),
        EngineFrame::CatalogMaintenanceReport { id, report } => (
            id,
            "catalog_maintenance_report",
            format!(
                "purged={} orphans={} fts={}",
                report.tombstones_purged,
                report.orphan_embeddings_removed,
                report.search_index_optimized
            ),
        ),
        EngineFrame::AiCacheBuildReport { id, report } => (
            id,
            "ai_cache_build_report",
            format!(
                "processed={} remaining={}",
                report.processed, report.remaining_stale
            ),
        ),
        EngineFrame::InvalidateAiCacheReport { id, report } => (
            id,
            "invalidate_ai_cache_report",
            format!(
                "invalidated={} embeddings_deleted={}",
                report.invalidated, report.embeddings_deleted
            ),
        ),
        EngineFrame::SchemaAnswer { id, response } => {
            let grounded = response
                .meta
                .get("grounded_in")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            (
                id,
                "schema_answer",
                format!("chars={} grounded={}", response.text.len(), grounded),
            )
        }
        EngineFrame::SchemaSearchResults { id, results } => (
            id,
            "schema_search_results",
            format!("results={}", results.len()),
        ),
        EngineFrame::JoinPathReport { id, paths } => {
            (id, "join_path_report", format!("paths={}", paths.len()))
        }
        EngineFrame::EmbeddingsBuildReport { id, report, .. } => (
            id,
            "embeddings_build_report",
            format!(
                "embedded={} failed={} remaining={}",
                report.embedded, report.failed, report.remaining_stale
            ),
        ),
        EngineFrame::SqlGenerationReport { id, report } => (
            id,
            "sql_generation_report",
            format!(
                "chars={} intent={} valid={} confirm={} issues={} grounded={} attempts={}",
                report.sql.len(),
                report.intent.as_str(),
                report.validation.is_valid,
                report.validation.requires_confirmation,
                report.validation.issues.len(),
                report.grounded_in.len(),
                report.attempts.len(),
            ),
        ),
        EngineFrame::SqlValidationReport { id, report } => {
            let errors = report
                .issues
                .iter()
                .filter(|i| i.severity == sqlly_ai::catalog::SqlIssueSeverity::Error)
                .count();
            let warnings = report
                .issues
                .iter()
                .filter(|i| i.severity == sqlly_ai::catalog::SqlIssueSeverity::Warning)
                .count();
            let confirm = report
                .issues
                .iter()
                .filter(|i| i.severity == sqlly_ai::catalog::SqlIssueSeverity::RequireConfirmation)
                .count();
            (
                id,
                "sql_validation_report",
                format!(
                    "valid={} intent={} errors={errors} warnings={warnings} confirm={confirm} objects={}",
                    report.is_valid,
                    report.detected_intent.as_str(),
                    report.referenced_objects.len(),
                ),
            )
        }
    }
}

/// Extract the request id from an outbound frame. Returns `None` for `Ready`
/// (which has no triggering request). Used by `write_frames_blocking` to
/// produce a best-effort `request_failed` when encoding a frame fails.
pub(crate) fn frame_request_id(frame: &EngineFrame) -> Option<&str> {
    match frame {
        EngineFrame::Ready { .. } => None,
        EngineFrame::Pong { id }
        | EngineFrame::RequestStarted { id }
        | EngineFrame::Event { id, .. }
        | EngineFrame::RowsBatch { id, .. }
        | EngineFrame::TruncatedCells { id, .. }
        | EngineFrame::RequestCompleted { id }
        | EngineFrame::RequestFailed { id, .. }
        | EngineFrame::RequestCancelled { id, .. }
        | EngineFrame::ShutdownAck { id }
        | EngineFrame::ValidationReport { id, .. }
        | EngineFrame::CatalogSyncReport { id, .. }
        | EngineFrame::CatalogMaintenanceReport { id, .. }
        | EngineFrame::ProviderStatusReport { id, .. }
        | EngineFrame::SqlReviewReport { id, .. }
        | EngineFrame::QuerySummaryReport { id, .. }
        | EngineFrame::RowEditReport { id, .. }
        | EngineFrame::AiCacheBuildReport { id, .. }
        | EngineFrame::InvalidateAiCacheReport { id, .. }
        | EngineFrame::SchemaAnswer { id, .. }
        | EngineFrame::SchemaSearchResults { id, .. }
        | EngineFrame::JoinPathReport { id, .. }
        | EngineFrame::EmbeddingsBuildReport { id, .. }
        | EngineFrame::SqlGenerationReport { id, .. }
        | EngineFrame::SqlValidationReport { id, .. } => Some(id),
    }
}

/// Pull the `id` and `type` fields out of a raw inbound request frame for the
/// REQ log line, WITHOUT requiring the frame to parse into a valid
/// [`EngineRequest`]. This means even a request that fails to deserialize still
/// gets a REQ line carrying whatever id it declared, so a malformed request is
/// attributable to the message that sent it. Falls back to `"unknown"` for the
/// id and `"?"` for the type when the field is absent or the body is too short.
pub(crate) fn raw_request_fields(frame: &[u8]) -> (String, String) {
    let kind = match frame.first().copied() {
        Some(REQUEST_TAG_PING) => "ping",
        Some(REQUEST_TAG_SHUTDOWN) => "shutdown",
        Some(REQUEST_TAG_CANCEL) => "cancel",
        Some(REQUEST_TAG_EXECUTE_QUERY) => "execute_query",
        Some(REQUEST_TAG_BUILD_AI_CACHE) => "build_ai_cache",
        Some(REQUEST_TAG_INVALIDATE_AI_CACHE) => "invalidate_ai_cache",
        Some(REQUEST_TAG_ASK_SCHEMA) => "ask_schema",
        Some(REQUEST_TAG_SEARCH_SCHEMA) => "search_schema",
        Some(REQUEST_TAG_FIND_JOIN_PATH) => "find_join_path",
        Some(REQUEST_TAG_BUILD_EMBEDDINGS) => "build_embeddings",
        Some(REQUEST_TAG_GENERATE_SQL) => "generate_sql",
        Some(REQUEST_TAG_VALIDATE_SQL) => "validate_sql",
        Some(REQUEST_TAG_VALIDATE) => "validate",
        Some(REQUEST_TAG_SYNC_CATALOG) => "sync_catalog",
        Some(REQUEST_TAG_ROW_EDIT) => "row_edit",
        Some(REQUEST_TAG_CATALOG_MAINTENANCE) => "catalog_maintenance",
        Some(REQUEST_TAG_PROVIDER_STATUS) => "provider_status",
        Some(REQUEST_TAG_REVIEW_SQL) => "review_sql",
        Some(REQUEST_TAG_SUMMARIZE_QUERY) => "summarize_query",
        _ => "?",
    };
    let id = if frame.is_empty() {
        "unknown".to_string()
    } else {
        best_effort_binary_id(&frame[1..]).unwrap_or_else(|| "unknown".to_string())
    };
    (id, kind.to_string())
}

/// Best-effort extraction of the request id from a binary request body. Used
/// only by `raw_request_fields` for logging when the body is binary — returns
/// `None` if the id cannot be read (e.g. the body is too short).
pub(crate) fn best_effort_binary_id(body: &[u8]) -> Option<String> {
    let mut r = BinaryFrameReader::new(body);
    r.read_u16_le("layout_version").ok()?;
    r.read_string("id").ok()
}

/// Best-effort id extraction from a binary request frame. All request frames
/// start with a tag byte, then u16 layout_version, then string id.
pub(crate) fn best_effort_request_id(frame: &[u8]) -> Option<String> {
    if frame.len() < 2 {
        return None;
    }
    best_effort_binary_id(&frame[1..])
}
