use super::*;
use sqlly_core::StudioError;
use sqlly_services::{
    CatalogService, DefaultCatalogService, TenantScopePolicy, catalog::parse_dialect,
};

pub(crate) const DEFAULT_ROW_BUFFER: usize = 256;

/// Resolve a request's connection through [`EnginePools::resolve_profile`]
/// inside `dispatch_request`; a refusal answers the request with
/// `request_failed` and keeps the engine serving.
macro_rules! resolve_profile {
    ($connection:expr, $id:expr, $frames:expr, $pool:expr) => {
        match $pool.resolve_profile(*$connection) {
            Ok(profile) => profile,
            Err(error) => {
                $frames
                    .send(EngineFrame::RequestFailed { id: $id, error })
                    .await;
                return true;
            }
        }
    };
}

/// Extract a human-readable message from a panic payload (`Box<dyn Any>`).
pub(crate) fn extract_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

/// Spawn an engine task that is resilient to panics. On panic, sends a
/// `request_failed` frame back to the client and always cleans up the active
/// task map — so a panicking task never leaves the client hanging or leaks the
/// entry.
pub(crate) fn spawn_panic_safe(
    id: String,
    frames: LoggedSender,
    active: ActiveTasks,
    body: impl std::future::Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    let task_id = id;
    let active_for_task = active;
    let frames_for_panic = frames.clone();

    tokio::spawn(async move {
        let result = AssertUnwindSafe(body).catch_unwind().await;

        if let Err(panic) = result {
            let msg = extract_panic_message(&*panic);
            eprintln!("[engine] task {task_id} panicked: {msg}");
            frames_for_panic
                .send(EngineFrame::RequestFailed {
                    id: task_id.clone(),
                    error: EngineError::internal(format!("engine task panicked: {msg}")),
                })
                .await;
        }

        active_for_task.lock().await.remove(&task_id);
    })
}

/// Parse and dispatch a single engine request frame. Returns `false` when the
/// engine should stop its read loop (a `Shutdown` request), `true` otherwise.
pub(crate) async fn dispatch_request(
    frame: &[u8],
    frames: &LoggedSender,
    active: &ActiveTasks,
    pool: &EnginePools,
    catalog: &DefaultCatalogService,
) -> bool {
    // Log the inbound request the moment it arrives — with its declared id,
    // type, and raw body — BEFORE parsing or any work. A request that then
    // crashes or wedges leaves this REQ line on disk with no terminal RES line
    // sharing its id, which pinpoints the failing message.
    if let Some(log) = &frames.log {
        let (id, kind) = raw_request_fields(frame);
        log.log_request(&id, &kind, &String::from_utf8_lossy(frame));
    }

    // All requests arrive as tagged binary frames. The first byte is the tag.
    // Unknown tags produce a graceful `request_failed` and the engine stays
    // alive — the frame length prefix already isolated this payload, so the
    // stream is not desynchronized.
    let request: Result<EngineRequest> = match frame.first().copied() {
        Some(REQUEST_TAG_PING) => decode_ping_binary(&frame[1..]),
        Some(REQUEST_TAG_SHUTDOWN) => decode_shutdown_binary(&frame[1..]),
        Some(REQUEST_TAG_CANCEL) => decode_cancel_binary(&frame[1..]),
        Some(REQUEST_TAG_EXECUTE_QUERY) => decode_execute_query_binary(&frame[1..]),
        Some(REQUEST_TAG_BUILD_AI_CACHE) => decode_build_ai_cache_binary(&frame[1..]),
        Some(REQUEST_TAG_INVALIDATE_AI_CACHE) => decode_invalidate_ai_cache_binary(&frame[1..]),
        Some(REQUEST_TAG_ASK_SCHEMA) => decode_ask_schema_binary(&frame[1..]),
        Some(REQUEST_TAG_SEARCH_SCHEMA) => decode_search_schema_binary(&frame[1..]),
        Some(REQUEST_TAG_FIND_JOIN_PATH) => decode_find_join_path_binary(&frame[1..]),
        Some(REQUEST_TAG_BUILD_EMBEDDINGS) => decode_build_embeddings_binary(&frame[1..]),
        Some(REQUEST_TAG_GENERATE_SQL) => decode_generate_sql_binary(&frame[1..]),
        Some(REQUEST_TAG_VALIDATE_SQL) => decode_validate_sql_binary(&frame[1..]),
        Some(REQUEST_TAG_VALIDATE) => decode_validate_binary(&frame[1..]),
        Some(REQUEST_TAG_SYNC_CATALOG) => decode_sync_catalog_binary(&frame[1..]),
        Some(REQUEST_TAG_ROW_EDIT) => decode_row_edit_binary(&frame[1..]),
        Some(REQUEST_TAG_CATALOG_MAINTENANCE) => decode_catalog_maintenance_binary(&frame[1..]),
        Some(REQUEST_TAG_PROVIDER_STATUS) => decode_provider_status_binary(&frame[1..]),
        Some(REQUEST_TAG_REVIEW_SQL) => decode_review_sql_binary(&frame[1..]),
        Some(REQUEST_TAG_SUMMARIZE_QUERY) => decode_summarize_query_binary(&frame[1..]),
        Some(tag) => Err(anyhow::anyhow!("unknown request tag 0x{tag:02x}")),
        None => Err(anyhow::anyhow!("empty request frame")),
    };

    let request = match request {
        Ok(request) => request,
        Err(error) => {
            let id = best_effort_request_id(frame).unwrap_or_else(|| "unknown".to_string());
            frames
                .send(EngineFrame::RequestFailed {
                    id,
                    error: EngineError::protocol(format!("invalid engine request: {error}")),
                })
                .await;
            return true;
        }
    };

    match request {
        EngineRequest::Ping { id } => {
            frames.send(EngineFrame::Pong { id }).await;
        }
        EngineRequest::ExecuteQuery {
            id,
            connection,
            sql,
            row_buffer,
            connection_key,
            cell_previews,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            // Proxy mode enforces the catalog's SELECT-only policy here, before
            // a driver is even chosen: the browser's copy of the flag is
            // untrusted, and a refused statement must never reach the server.
            if pool.proxy.is_some() && profile.block_all_but_select {
                let verdict = sqlly_intel::query::safety::classify_select_only_for(
                    profile.database_type,
                    &sql,
                );
                if !verdict.select_only {
                    frames
                        .send(EngineFrame::RequestFailed {
                            id,
                            error: read_only_policy_error(verdict.reason.as_deref()),
                        })
                        .await;
                    return true;
                }
            }
            let buffer = row_buffer.unwrap_or(DEFAULT_ROW_BUFFER).max(1);
            let pool = pool.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let cancel_slot: sqlly_services::CancelSlot = Arc::new(std::sync::Mutex::new(None));
            pool.query_cancels
                .lock()
                .await
                .insert(id.clone(), cancel_slot.clone());
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                run_query(
                    task_id,
                    profile,
                    sql,
                    buffer,
                    task_frames,
                    pool,
                    connection_key,
                    cancel_slot,
                    cell_previews,
                )
                .await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::Cancel { id, target_id } => {
            // Server-side first: the registered hook (KILL <spid>, PG wire
            // cancel, MySQL / ClickHouse KILL QUERY, SQLite / DuckDB
            // interrupt, Oracle break) stops the statement on the server;
            // aborting first would drop the client the cancel addresses.
            // libSQL has no kill primitive, so its hook only drops the
            // in-flight request; Redis releases the running command and
            // drops the connection.
            if let Some(slot) = pool.query_cancels.lock().await.remove(&target_id)
                && let Some(hook) = slot.lock().expect("cancel slot poisoned").take()
            {
                hook();
            }
            if let Some(handle) = active.lock().await.remove(&target_id) {
                handle.abort();
            }
            frames
                .send(EngineFrame::RequestCancelled { id, target_id })
                .await;
        }
        EngineRequest::Shutdown { id } => {
            frames.send(EngineFrame::ShutdownAck { id }).await;
            return false;
        }
        EngineRequest::Validate {
            id,
            sql,
            dialect,
            default_schema,
            objects,
            foreign_keys,
        } => {
            let frame = run_validation(id, sql, dialect, default_schema, objects, foreign_keys);
            frames.send(frame).await;
        }
        EngineRequest::SyncCatalog {
            id,
            connection,
            objects,
            coverage,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            // All local SQLite I/O; runs inline like Validate (sub-ms for
            // typical schemas). The store diffs the incoming objects against
            // persisted state and invalidates AI cards for changed objects.
            let frame = run_sync_catalog(id, &profile, catalog, &objects, coverage).await;
            frames.send(frame).await;
        }
        EngineRequest::CatalogMaintenance {
            id,
            connection,
            options,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let frame = match catalog.catalog_maintenance(&profile, options).await {
                Ok(report) => EngineFrame::CatalogMaintenanceReport { id, report },
                Err(error) => EngineFrame::RequestFailed {
                    id,
                    error: EngineError::catalog(error.to_string()),
                },
            };
            frames.send(frame).await;
        }
        EngineRequest::ProviderStatus { id } => {
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame = match catalog.provider_status().await {
                    Ok(report) => EngineFrame::ProviderStatusReport {
                        id: task_id,
                        report,
                    },
                    Err(error) => EngineFrame::RequestFailed {
                        id: task_id,
                        error: EngineError::provider(error.to_string()),
                    },
                };
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::ReviewSql {
            id,
            connection,
            sql,
            dialect,
            max_tokens,
            tenant_scope,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame = run_review_sql(
                    task_id,
                    profile,
                    &catalog,
                    sql,
                    dialect,
                    max_tokens,
                    tenant_scope,
                )
                .await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::SummarizeQuery {
            id,
            connection,
            sql,
            result_columns,
            row_count,
            elapsed_ms,
            max_tokens,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame = run_summarize_query(
                    task_id,
                    profile,
                    &catalog,
                    sql,
                    result_columns,
                    row_count,
                    elapsed_ms,
                    max_tokens,
                )
                .await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::BuildAiCache { id, connection } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame = run_build_ai_cache(task_id, profile, &catalog).await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::RowEdit {
            id,
            connection,
            op,
            payload,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            // Row editing is a data mutation: a SELECT-only proxied connection
            // refuses every row-edit op outright.
            if pool.proxy.is_some() && profile.block_all_but_select {
                frames
                    .send(EngineFrame::RequestFailed {
                        id,
                        error: read_only_policy_error(Some("row editing changes data")),
                    })
                    .await;
                return true;
            }
            let task_frames = frames.clone();
            let task_id = id.clone();
            let task_pool = pool.clone();
            let cancel_slot: sqlly_services::CancelSlot = Arc::new(std::sync::Mutex::new(None));
            pool.query_cancels
                .lock()
                .await
                .insert(id.clone(), cancel_slot.clone());
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame =
                    run_row_edit(task_id, profile, &op, &payload, cancel_slot, task_pool).await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::InvalidateAiCache { id, connection } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let frame = run_invalidate_ai_cache(id, &profile, catalog).await;
            frames.send(frame).await;
        }
        EngineRequest::AskSchema {
            id,
            connection,
            question,
            max_tokens,
            system,
            purpose,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame = run_ask_schema(
                    task_id, profile, &catalog, question, max_tokens, system, purpose,
                )
                .await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::SearchSchema {
            id,
            connection,
            query,
            limit,
            object_types,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame =
                    run_search_schema(task_id, profile, &catalog, query, limit, object_types).await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::FindJoinPath {
            id,
            connection,
            source,
            target,
            max_depth,
            include_inferred,
            min_confidence,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let options = sqlly_ai::catalog::JoinPathOptions {
                max_depth,
                include_inferred,
                min_confidence,
            };
            let frame = run_find_join_path(id, &profile, catalog, source, target, options).await;
            frames.send(frame).await;
        }
        EngineRequest::BuildEmbeddings { id, connection } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame = run_build_embeddings(task_id, profile, &catalog).await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::GenerateSql {
            id,
            connection,
            question,
            dialect,
            max_tokens,
            system,
            max_attempts,
            tenant_scope,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let catalog = catalog.clone();
            let task_frames = frames.clone();
            let task_id = id.clone();
            let handle = spawn_panic_safe(id.clone(), frames.clone(), active.clone(), async move {
                let frame = run_generate_sql(
                    task_id,
                    profile,
                    &catalog,
                    question,
                    dialect,
                    max_tokens,
                    system,
                    max_attempts,
                    tenant_scope,
                )
                .await;
                task_frames.send(frame).await;
            });
            active.lock().await.insert(id, handle);
        }
        EngineRequest::ValidateSql {
            id,
            connection,
            sql,
            dialect,
            tenant_scope,
        } => {
            let profile = resolve_profile!(connection, id, frames, pool);
            let frame = run_validate_sql(id, &profile, catalog, sql, dialect, tenant_scope);
            frames.send(frame).await;
        }
    }
    true
}

/// Legacy modelengine validation shim (plan 064). Serves `REQUEST_TAG_VALIDATE`
/// (0x10) for frozen Swift. Canonical validation is [`run_validate_sql`]
/// (tag 0x0C, catalog `validate_sql`). No database I/O — snapshot comes from
/// the request payload.
pub(crate) fn run_validation(
    id: String,
    sql: String,
    dialect: Option<String>,
    default_schema: Option<String>,
    objects: Vec<crate::intellisense_cmd::SchemaObject>,
    foreign_keys: Vec<crate::intellisense_cmd::SchemaForeignKey>,
) -> EngineFrame {
    let dialect = match crate::intellisense_cmd::parse_dialect(dialect.as_deref()) {
        Ok(dialect) => dialect,
        Err(error) => {
            return EngineFrame::RequestFailed {
                id,
                error: EngineError::protocol(format!("invalid dialect: {error}")),
            };
        }
    };

    let snapshot = crate::intellisense_cmd::build_snapshot(&objects, &foreign_keys, dialect);
    let namespace = default_schema
        .filter(|schema| !schema.is_empty())
        .unwrap_or_else(|| "dbo".to_string());
    let request = QueryValidationRequest {
        text: sql.clone(),
        dialect,
        default_catalog: None,
        default_namespace: Some(SqlName::new(&namespace, dialect, false)),
        scope: ValidationScope::Model,
    };
    let mode = EngineMode::Edit {
        exemptions: ExemptionSet::default(),
    };
    let report = validate_query(&request, &snapshot, &mode);

    let diagnostics = report
        .issues
        .into_iter()
        .map(|issue| WireDiag {
            severity: wire_severity(issue.severity),
            class: wire_class(issue.class),
            span_start: issue.span.start as u32,
            span_len: issue.span.end.saturating_sub(issue.span.start) as u32,
            line: issue.line,
            col: issue.column,
            message: issue.message,
        })
        .collect();

    let resolved = report
        .resolved
        .into_iter()
        .filter(|resolution| resolution.resolved_to.is_some())
        .map(|resolution| WireSpan {
            start: resolution.span.start as u32,
            len: resolution.span.end.saturating_sub(resolution.span.start) as u32,
        })
        .collect();

    EngineFrame::ValidationReport {
        id,
        diagnostics,
        resolved,
    }
}

/// Run a catalog sync through the shared catalog service.
pub(crate) async fn run_sync_catalog(
    id: String,
    profile: &ConnectionProfile,
    catalog: &DefaultCatalogService,
    objects: &[CatalogObject],
    coverage: sqlly_ai::catalog::SyncCoverage,
) -> EngineFrame {
    match catalog
        .sync_catalog(profile, objects.to_vec(), coverage)
        .await
    {
        Ok(report) => EngineFrame::CatalogSyncReport { id, report },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_catalog_error(error),
        },
    }
}

/// The engine-wide row-editing service: one warm-connection pool shared by
/// every row-edit request, mirroring how the app's in-process services share
/// theirs. Initialized on first use; the reaper runs for the process lifetime.
pub(crate) fn row_editing_service() -> &'static sqlly_services::row_editing::DefaultRowEditingService
{
    use std::sync::OnceLock;
    static SERVICE: OnceLock<sqlly_services::row_editing::DefaultRowEditingService> =
        OnceLock::new();
    SERVICE.get_or_init(|| {
        let pool = sqlly_services::tds_pool::TdsPool::new();
        pool.spawn_reaper();
        sqlly_services::row_editing::DefaultRowEditingService::new(pool)
    })
}

/// Most keys one `fk_labels` row-edit request may carry. The app chunks at
/// 500; anything larger is a broken or hostile peer.
pub(crate) const ROW_EDIT_FK_LABELS_MAX_KEYS: usize = 1000;

/// A `fetch_row` request over the wire: the op has bare arguments rather than
/// a request struct, so this carries them.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct FetchRowPayload {
    target: sqlly_services::row_editing::TableTarget,
    key: Vec<(String, sqlly_core::CellValue)>,
}

/// Execute one row-editing op engine-side through the SAME
/// `DefaultRowEditingService` the app uses in-process — identical provenance
/// discovery, primary-key guards, optimistic concurrency, and transactional
/// single-row statements. The op name selects the service method; the payload
/// is that method's request as JSON; the report is its outcome as JSON.
pub(crate) async fn run_row_edit(
    id: String,
    profile: ConnectionProfile,
    op: &str,
    payload: &str,
    cancel_slot: sqlly_services::CancelSlot,
    pools: EnginePools,
) -> EngineFrame {
    use sqlly_services::row_editing::RowEditingService;
    let service = row_editing_service();

    fn bad_payload(id: String, op: &str, e: impl std::fmt::Display) -> EngineFrame {
        EngineFrame::RequestFailed {
            id,
            error: EngineError::protocol(format!("row_edit {op}: invalid payload: {e}")),
        }
    }

    macro_rules! run_op {
        ($request_ty:ty, $method:ident) => {{
            let request: $request_ty = match serde_json::from_str(payload) {
                Ok(request) => request,
                Err(e) => return bad_payload(id.clone(), op, e),
            };
            match service.$method(&profile, request).await {
                Ok(outcome) => match serde_json::to_value(&outcome) {
                    Ok(report) => EngineFrame::RowEditReport {
                        id: id.clone(),
                        report,
                    },
                    Err(e) => EngineFrame::RequestFailed {
                        id: id.clone(),
                        error: EngineError::protocol(format!("row_edit {op}: encode outcome: {e}")),
                    },
                },
                Err(error) => EngineFrame::RequestFailed {
                    id: id.clone(),
                    error: EngineError::query(format!("row_edit {op}: {error}")),
                },
            }
        }};
    }

    macro_rules! run_cancellable_op {
        ($request_ty:ty, $method:ident) => {{
            let request: $request_ty = match serde_json::from_str(payload) {
                Ok(request) => request,
                Err(e) => return bad_payload(id.clone(), op, e),
            };
            match service.$method(&profile, request, &cancel_slot).await {
                Ok(outcome) => match serde_json::to_value(&outcome) {
                    Ok(report) => EngineFrame::RowEditReport {
                        id: id.clone(),
                        report,
                    },
                    Err(e) => EngineFrame::RequestFailed {
                        id: id.clone(),
                        error: EngineError::protocol(format!("row_edit {op}: encode outcome: {e}")),
                    },
                },
                Err(error) => EngineFrame::RequestFailed {
                    id: id.clone(),
                    error: EngineError::query(format!("row_edit {op}: {error}")),
                },
            }
        }};
    }

    let frame = match op {
        "open_session" => {
            run_op!(
                sqlly_services::row_editing::RowEditOpenRequest,
                open_session
            )
        }
        "open_insert_session" => run_op!(
            sqlly_services::row_editing::RowEditOpenRequest,
            open_insert_session
        ),
        "update_row" => run_cancellable_op!(
            sqlly_services::row_editing::UpdateRowRequest,
            update_row_cancellable
        ),
        "insert_row" => run_cancellable_op!(
            sqlly_services::row_editing::InsertRowRequest,
            insert_row_cancellable
        ),
        "delete_row" => run_cancellable_op!(
            sqlly_services::row_editing::DeleteRowRequest,
            delete_row_cancellable
        ),
        // The staged grid save: one transaction, each statement's
        // expected affected-row count checked, rolled back on a mismatch or
        // error — the same `DefaultRowEditingService` path a local save runs.
        "apply_batch" => run_cancellable_op!(
            sqlly_services::row_editing::BatchRequest,
            apply_batch_cancellable
        ),
        "fk_search" => run_op!(sqlly_services::row_editing::FkSearchRequest, fk_search),
        "fk_labels" => {
            let request: sqlly_services::row_editing::FkLabelRequest =
                match serde_json::from_str(payload) {
                    Ok(request) => request,
                    Err(e) => return bad_payload(id.clone(), op, e),
                };
            // The client chunks at 500 keys; refuse an oversized peer request
            // instead of building an unbounded IN list from it.
            if request.keys.len() > ROW_EDIT_FK_LABELS_MAX_KEYS {
                return bad_payload(
                    id.clone(),
                    op,
                    format!(
                        "{} keys exceeds the {ROW_EDIT_FK_LABELS_MAX_KEYS}-key limit",
                        request.keys.len()
                    ),
                );
            }
            match service.fk_labels(&profile, request).await {
                Ok(outcome) => match serde_json::to_value(&outcome) {
                    Ok(report) => EngineFrame::RowEditReport {
                        id: id.clone(),
                        report,
                    },
                    Err(e) => EngineFrame::RequestFailed {
                        id: id.clone(),
                        error: EngineError::protocol(format!("row_edit {op}: encode outcome: {e}")),
                    },
                },
                Err(error) => EngineFrame::RequestFailed {
                    id: id.clone(),
                    error: EngineError::query(format!("row_edit {op}: {error}")),
                },
            }
        }
        "related_rows" => run_op!(
            sqlly_services::row_editing::RelatedRowsRequest,
            related_rows
        ),
        "fetch_row" => {
            let request: FetchRowPayload = match serde_json::from_str(payload) {
                Ok(request) => request,
                Err(e) => return bad_payload(id.clone(), op, e),
            };
            match service
                .fetch_row(&profile, &request.target, &request.key)
                .await
            {
                Ok(outcome) => match serde_json::to_value(&outcome) {
                    Ok(report) => EngineFrame::RowEditReport {
                        id: id.clone(),
                        report,
                    },
                    Err(e) => EngineFrame::RequestFailed {
                        id: id.clone(),
                        error: EngineError::protocol(format!(
                            "row_edit fetch_row: encode outcome: {e}"
                        )),
                    },
                },
                Err(error) => EngineFrame::RequestFailed {
                    id: id.clone(),
                    error: EngineError::query(format!("row_edit fetch_row: {error}")),
                },
            }
        }
        other => EngineFrame::RequestFailed {
            id: id.clone(),
            error: EngineError::protocol(format!("unknown row_edit op `{other}`")),
        },
    };
    pools.query_cancels.lock().await.remove(&id);
    frame
}

/// Build the AI cache for every stale object in this connection's catalog.
/// Runs as an async task: generating cards calls the async LLM provider once
/// per stale object, asking it to describe each object with a concise card and
/// falling back to the compact card text on provider error.
pub(crate) async fn run_build_ai_cache(
    id: String,
    profile: ConnectionProfile,
    catalog: &DefaultCatalogService,
) -> EngineFrame {
    match catalog.build_ai_cache_report(&profile).await {
        Ok(report) => EngineFrame::AiCacheBuildReport { id, report },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_provider_error(error, "build ai cache"),
        },
    }
}

/// Nuke all AI-derived data in this connection's catalog.
pub(crate) async fn run_invalidate_ai_cache(
    id: String,
    profile: &ConnectionProfile,
    catalog: &DefaultCatalogService,
) -> EngineFrame {
    match catalog.invalidate_ai_cache(profile).await {
        Ok(report) => EngineFrame::InvalidateAiCacheReport { id, report },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_catalog_error(error),
        },
    }
}

/// Answer a natural-language question against the persisted catalog.
pub(crate) async fn run_ask_schema(
    id: String,
    profile: ConnectionProfile,
    catalog: &DefaultCatalogService,
    question: String,
    max_tokens: Option<usize>,
    system: Option<String>,
    purpose: SchemaPurpose,
) -> EngineFrame {
    let ask = catalog.ask_schema(&profile, &question, max_tokens, system, purpose);
    let result = match schema_llm_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, ask).await {
            Ok(result) => result,
            Err(_) => {
                return EngineFrame::RequestFailed {
                    id,
                    error: EngineError::timeout(schema_llm_timeout_message("ask schema", timeout)),
                };
            }
        },
        None => ask.await,
    };
    match result {
        Ok(mut response) => {
            if let Some(note) = catalog.diagnostics().llm_fallback_note() {
                let prefixed = format!("[{note}]");
                if response.text.is_empty() {
                    response.text = prefixed;
                } else {
                    response.text = format!("{prefixed}\n\n{}", response.text);
                }
            }
            EngineFrame::SchemaAnswer { id, response }
        }
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_provider_error(error, "ask schema"),
        },
    }
}

/// Schema search (FTS-only or hybrid when embeddings exist).
pub(crate) async fn run_search_schema(
    id: String,
    profile: ConnectionProfile,
    catalog: &DefaultCatalogService,
    query: String,
    limit: Option<usize>,
    object_types: Vec<String>,
) -> EngineFrame {
    match catalog
        .search_schema(&profile, &query, limit, object_types)
        .await
    {
        Ok(results) => EngineFrame::SchemaSearchResults { id, results },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_catalog_error(error),
        },
    }
}

pub(crate) async fn run_find_join_path(
    id: String,
    profile: &ConnectionProfile,
    catalog: &DefaultCatalogService,
    source: String,
    target: String,
    options: sqlly_ai::catalog::JoinPathOptions,
) -> EngineFrame {
    match catalog
        .find_join_path(profile, &source, &target, options)
        .await
    {
        Ok(paths) => EngineFrame::JoinPathReport { id, paths },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_catalog_error(error),
        },
    }
}

/// Map optional tenant-scope wire fields onto catalog tenant policy.
fn tenant_policy_from_wire(
    tenant_scope: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
) -> TenantScopePolicy {
    match tenant_scope {
        None => TenantScopePolicy::default(),
        Some(scope) => TenantScopePolicy {
            disabled: !scope.require_tenant_filter,
            fail_closed: scope.fail_closed_tenant_filter,
            tenant_column_folds: scope.tenant_column_folds,
        },
    }
}

fn studio_catalog_error(error: StudioError) -> EngineError {
    EngineError::catalog(error.to_string())
}

fn studio_provider_error(error: StudioError, action: &str) -> EngineError {
    EngineError::provider(format!("{action}: {}", error))
}

fn parse_wire_dialect(
    dialect: Option<String>,
) -> Result<sqlly_lang::sqlmodel::Dialect, EngineError> {
    parse_dialect(dialect.as_deref())
        .map_err(|error| EngineError::protocol(format!("invalid dialect: {error}")))
}

/// Generate SQL from a natural-language question grounded in the persisted
/// catalog, then validate it. Runs as an async task off the dispatch loop
/// because the underlying context-assembly path calls the LLM provider.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_generate_sql(
    id: String,
    profile: ConnectionProfile,
    catalog: &DefaultCatalogService,
    question: String,
    dialect: Option<String>,
    max_tokens: Option<usize>,
    system: Option<String>,
    max_attempts: Option<usize>,
    tenant_scope: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
) -> EngineFrame {
    let dialect = match parse_wire_dialect(dialect) {
        Ok(dialect) => dialect,
        Err(error) => {
            return EngineFrame::RequestFailed { id, error };
        }
    };
    let tenant = tenant_policy_from_wire(tenant_scope);
    let generate = catalog.generate_sql_with_policy(
        &profile,
        &question,
        dialect,
        max_tokens,
        system,
        max_attempts,
        tenant,
    );
    let result = match schema_llm_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, generate).await {
            Ok(result) => result,
            Err(_) => {
                return EngineFrame::RequestFailed {
                    id,
                    error: EngineError::timeout(schema_llm_timeout_message(
                        "generate sql",
                        timeout,
                    )),
                };
            }
        },
        None => generate.await,
    };
    match result {
        Ok(report) => EngineFrame::SqlGenerationReport { id, report },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_provider_error(error, "generate sql"),
        },
    }
}

/// **Canonical validation path (plan 064):** validate against the persisted
/// catalog. Serves `REQUEST_TAG_VALIDATE_SQL` (0x0C).
pub(crate) fn run_validate_sql(
    id: String,
    profile: &ConnectionProfile,
    catalog: &DefaultCatalogService,
    sql: String,
    dialect: Option<String>,
    tenant_scope: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
) -> EngineFrame {
    let dialect = match parse_wire_dialect(dialect) {
        Ok(dialect) => dialect,
        Err(error) => {
            return EngineFrame::RequestFailed { id, error };
        }
    };
    let tenant = tenant_policy_from_wire(tenant_scope);
    match catalog.validate_sql_with_policy(profile, &sql, dialect, tenant) {
        Ok(report) => EngineFrame::SqlValidationReport { id, report },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_catalog_error(error),
        },
    }
}

/// Catalog-grounded SQL review through the shared catalog service.
pub(crate) async fn run_review_sql(
    id: String,
    profile: ConnectionProfile,
    catalog: &DefaultCatalogService,
    sql: String,
    dialect: Option<String>,
    max_tokens: Option<usize>,
    tenant_scope: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
) -> EngineFrame {
    let dialect = match parse_wire_dialect(dialect) {
        Ok(dialect) => dialect,
        Err(error) => {
            return EngineFrame::RequestFailed { id, error };
        }
    };
    let tenant = tenant_policy_from_wire(tenant_scope);
    let budget = max_tokens.unwrap_or(sqlly_ai::catalog::DEFAULT_CONTEXT_TOKENS);
    let review = catalog.review_sql_with_policy(&profile, &sql, dialect, tenant, budget);
    let result = match schema_llm_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, review).await {
            Ok(result) => result,
            Err(_) => {
                return EngineFrame::RequestFailed {
                    id,
                    error: EngineError::timeout(schema_llm_timeout_message("review sql", timeout)),
                };
            }
        },
        None => review.await,
    };
    match result {
        Ok(mut report) => {
            if let Some(note) = catalog.diagnostics().llm_fallback_note() {
                report.narrative = format!("[{note}] {}", report.narrative);
            }
            EngineFrame::SqlReviewReport { id, report }
        }
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_provider_error(error, "review sql"),
        },
    }
}

/// Catalog-grounded query summary through the shared catalog service.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_summarize_query(
    id: String,
    profile: ConnectionProfile,
    catalog: &DefaultCatalogService,
    sql: String,
    result_columns: Vec<String>,
    row_count: Option<u64>,
    elapsed_ms: Option<u64>,
    max_tokens: Option<usize>,
) -> EngineFrame {
    let budget = max_tokens.unwrap_or(sqlly_ai::catalog::DEFAULT_CONTEXT_TOKENS);
    let summary = catalog.summarize_query_with_budget(
        &profile,
        &sql,
        &result_columns,
        row_count,
        elapsed_ms,
        budget,
    );
    let result = match schema_llm_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, summary).await {
            Ok(result) => result,
            Err(_) => {
                return EngineFrame::RequestFailed {
                    id,
                    error: EngineError::timeout(schema_llm_timeout_message(
                        "summarize query",
                        timeout,
                    )),
                };
            }
        },
        None => summary.await,
    };
    match result {
        Ok(mut report) => {
            if let Some(note) = catalog.diagnostics().llm_fallback_note() {
                report.summary = format!("[{note}] {}", report.summary);
            }
            EngineFrame::QuerySummaryReport { id, report }
        }
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_provider_error(error, "summarize query"),
        },
    }
}

/// Build the embedding cache through the shared catalog service.
pub(crate) async fn run_build_embeddings(
    id: String,
    profile: ConnectionProfile,
    catalog: &DefaultCatalogService,
) -> EngineFrame {
    match catalog.build_embeddings(&profile).await {
        Ok(report) => EngineFrame::EmbeddingsBuildReport {
            id,
            report,
            note: catalog.diagnostics().embedding_fallback_note(),
        },
        Err(error) => EngineFrame::RequestFailed {
            id,
            error: studio_provider_error(error, "build embeddings"),
        },
    }
}

#[allow(clippy::too_many_arguments)] // mirrors run_query_producer's surface
pub(crate) async fn run_query(
    id: String,
    profile: ConnectionProfile,
    sql: String,
    row_buffer: usize,
    frames: LoggedSender,
    pools: EnginePools,
    connection_key: Option<String>,
    cancel_slot: sqlly_services::CancelSlot,
    cell_previews: bool,
) {
    frames
        .send(EngineFrame::RequestStarted { id: id.clone() })
        .await;

    // Capture the connection timeout before `profile` is moved into the query
    // task; it doubles as the "no first response" watchdog deadline below.
    let connect_deadline = profile.timeout();

    let events = EventBus::new(1024);
    let mut subscription = events.subscribe();
    let (rows_tx, rows_rx) = sqlly_core::row_channel(row_buffer);
    let query_events = events.clone();

    // The producer dispatches on the profile's engine family and owns the
    // driver client end-to-end: warm checkout/checkin, the stale-warm retry
    // contract, and server-side cancel registration into `cancel_slot` all
    // live in `sqlly_services::run_query_producer`, shared with the direct
    // app path. If this task is aborted (Cancel), the client is dropped and
    // never re-pooled.
    let producer_pools = pools.clone();
    let producer_cancel_slot = cancel_slot.clone();
    let query: JoinHandle<StudioResult<Option<String>>> = tokio::spawn(async move {
        sqlly_services::run_query_producer(
            &profile,
            &sql,
            &query_events,
            &rows_tx,
            &producer_pools.tds,
            &producer_pools.pg,
            &producer_pools.mysql,
            connection_key.as_deref(),
            &producer_cancel_slot,
        )
        .await
    });
    drop(events);

    let (connected, event_error, stream_failed) = consume_query_stream(
        id.clone(),
        &frames,
        &mut subscription,
        rows_rx,
        connect_deadline,
        &cancel_slot,
        cell_previews,
    )
    .await;

    if stream_failed {
        query.abort();
        pools.query_cancels.lock().await.remove(&id);
        return;
    }

    if !connected {
        // No lifecycle activity arrived within the connection deadline and the
        // streams never closed: the underlying driver is wedged (e.g. stuck
        // mid-handshake — a state that, on the shared runtime, can starve
        // timers so an inner connect timeout never fires). Abort the orphaned
        // task and report failure so the client is never left waiting forever.
        // Aborting drops the client, so the wedged connection is never pooled.
        query.abort();
        pools.query_cancels.lock().await.remove(&id);
        frames
            .send(EngineFrame::RequestFailed {
                id,
                error: EngineError::timeout(format!(
                    "connection timed out after {}s with no response from server",
                    connect_deadline.as_secs()
                )),
            })
            .await;
        return;
    }

    let outcome = query.await;
    pools.query_cancels.lock().await.remove(&id);
    match outcome {
        Ok(Ok(first_error)) => {
            // TDS surfaces the first batch error out-of-band; the other
            // engines publish it as an Error event captured by the stream
            // consumer. Either way the request is reported failed with the
            // real message — the healthy client was still re-pooled by the
            // producer, since a recoverable batch error does not poison the
            // connection.
            match first_error.or(event_error) {
                Some(error) => {
                    frames
                        .send(EngineFrame::RequestFailed {
                            id,
                            error: EngineError::query(error),
                        })
                        .await;
                }
                None => {
                    frames.send(EngineFrame::RequestCompleted { id }).await;
                }
            }
        }
        Ok(Err(error)) => {
            frames
                .send(EngineFrame::RequestFailed {
                    id,
                    error: query_failure(error),
                })
                .await;
        }
        Err(error) if error.is_cancelled() => {
            frames
                .send(EngineFrame::RequestFailed {
                    id,
                    error: EngineError::query("query cancelled"),
                })
                .await;
        }
        Err(error) => {
            frames
                .send(EngineFrame::RequestFailed {
                    id,
                    error: EngineError::internal(format!("query task failed: {error}")),
                })
                .await;
        }
    }
}

/// The `request_failed` error for a query that returned a [`StudioError`].
/// A capability refusal — plain or keyed — travels as `unsupported` with its
/// detail only (no `Server capability not supported:` prefix), so the
/// client's `decode_failed_studio_error` rebuilds a `CapabilityNotSupported`
/// whose `Display` matches the engine-side error exactly.
pub(crate) fn query_failure(error: StudioError) -> EngineError {
    match error {
        StudioError::CapabilityNotSupported(_) | StudioError::CapabilityNotSupportedMessage(_) => {
            EngineError::unsupported(error.detail())
        }
        timeout @ StudioError::QueryTimeout(_) => EngineError::timeout(timeout.to_string()),
        other => EngineError::query(other.to_string()),
    }
}

/// Drives the lifecycle-event subscription and the bounded row receiver until
/// the query task finishes (both channels close). Returns `(connected,
/// first_error)`: `connected` is `false` if the connection produced no
/// activity at all within `connect_deadline` — a wedged-handshake guard so the
/// engine always emits a terminal frame even when the driver never responds
/// (once any lifecycle event or row batch arrives, the watchdog disarms so
/// legitimately long-running queries are never interrupted) — and
/// `first_error` carries the first Error event published on the stream, which
/// decides the terminal frame for drivers that report batch failures only as
/// events.
pub(crate) async fn consume_query_stream(
    id: String,
    frames: &LoggedSender,
    subscription: &mut sqlly_core::EventSubscription,
    mut rows_rx: RowReceiver,
    connect_deadline: std::time::Duration,
    cancel_slot: &sqlly_services::CancelSlot,
    cell_previews: bool,
) -> (bool, Option<String>, bool) {
    use tokio::sync::broadcast::error::RecvError;

    // Rows seen per result set, so a preview cut reports its absolute row.
    let mut rows_seen: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

    let mut events_done = false;
    let mut rows_done = false;
    let mut activity = false;
    let mut first_error: Option<String> = None;
    // Lifecycle events and rows travel on separate channels, and clients drop
    // `Rows` for a result set whose `ResultSetStarted` they haven't seen — a
    // small result's only batch winning the race showed as headers with zero
    // rows. So, like the in-process drain (`drain_query_stream_with`): prefer
    // events, hold rows until their set has started, and send the terminal
    // events (completed / cancelled / error) only after every row.
    let mut started_sets: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut pending_rows: Vec<sqlly_core::RowBatch> = Vec::new();
    let mut deferred_terminal: Vec<sqlly_core::Event> = Vec::new();
    let watchdog = tokio::time::sleep(connect_deadline);
    tokio::pin!(watchdog);
    loop {
        let batch = tokio::select! {
            biased;
            event = subscription.recv(), if !events_done => match event {
                Ok(event) => {
                    activity = true;
                    if first_error.is_none()
                        && let sqlly_core::EventPayload::Error { error } = &event.payload
                    {
                        first_error = Some(error.clone());
                    }
                    match &event.payload {
                        sqlly_core::EventPayload::QueryCompleted { .. }
                        | sqlly_core::EventPayload::QueryCancelled { .. }
                        | sqlly_core::EventPayload::Error { .. } => {
                            deferred_terminal.push(event);
                            None
                        }
                        sqlly_core::EventPayload::ResultSetStarted { result_set_index, .. } => {
                            let index = *result_set_index;
                            started_sets.insert(index);
                            frames.send(EngineFrame::Event { id: id.clone(), event }).await;
                            // Release the batches that raced ahead of it, in order.
                            let (ready, held): (Vec<_>, Vec<_>) = std::mem::take(&mut pending_rows)
                                .into_iter()
                                .partition(|b| b.result_set_index == index);
                            pending_rows = held;
                            for batch in ready {
                                if !send_rows_batch(&id, frames, batch, &mut rows_seen, cancel_slot, cell_previews).await {
                                    return (true, first_error, true);
                                }
                            }
                            None
                        }
                        _ => {
                            frames.send(EngineFrame::Event { id: id.clone(), event }).await;
                            None
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    activity = true;
                    continue;
                }
                Err(RecvError::Closed) => {
                    events_done = true;
                    None
                }
            },
            batch = rows_rx.recv(), if !rows_done => match batch {
                Some(batch) => {
                    activity = true;
                    if started_sets.contains(&batch.result_set_index) {
                        Some(batch)
                    } else {
                        pending_rows.push(batch);
                        None
                    }
                }
                None => {
                    rows_done = true;
                    None
                }
            },
            _ = &mut watchdog, if !activity => {
                return (false, first_error, false);
            }
            else => break,
        };

        if let Some(batch) = batch
            && !send_rows_batch(
                &id,
                frames,
                batch,
                &mut rows_seen,
                cancel_slot,
                cell_previews,
            )
            .await
        {
            return (true, first_error, true);
        }

        if events_done && rows_done {
            break;
        }
    }

    // A producer that died between sending rows and their lifecycle event:
    // still deliver the data rather than lose it silently.
    for batch in pending_rows {
        if !send_rows_batch(
            &id,
            frames,
            batch,
            &mut rows_seen,
            cancel_slot,
            cell_previews,
        )
        .await
        {
            return (true, first_error, true);
        }
    }
    for event in deferred_terminal {
        frames
            .send(EngineFrame::Event {
                id: id.clone(),
                event,
            })
            .await;
    }
    (true, first_error, false)
}

/// Forward one row batch (with its preview cut report). `false` means the
/// batch could not be framed: the request has been failed and the query
/// cancelled, and the caller must stop.
async fn send_rows_batch(
    id: &str,
    frames: &LoggedSender,
    mut batch: sqlly_core::RowBatch,
    rows_seen: &mut std::collections::HashMap<usize, usize>,
    cancel_slot: &sqlly_services::CancelSlot,
    cell_previews: bool,
) -> bool {
    let index = batch.result_set_index;
    let seen = rows_seen.entry(index).or_insert(0);
    let cut = if cell_previews {
        sqlly_services::truncate_rows(
            &mut batch.rows,
            *seen,
            &sqlly_services::CellLimits::default(),
        )
    } else {
        Vec::new()
    };
    *seen += batch.rows.len();
    match split_rows_batch(id, index, &batch.rows) {
        Ok(chunks) => {
            for chunk in chunks {
                frames
                    .send(EngineFrame::RowsBatch {
                        id: id.to_string(),
                        result_set_index: index,
                        rows: chunk.to_vec(),
                    })
                    .await;
            }
            if !cut.is_empty() {
                frames
                    .send(EngineFrame::TruncatedCells {
                        id: id.to_string(),
                        result_set_index: index,
                        cells: cut,
                    })
                    .await;
            }
            true
        }
        Err(e) => {
            frames
                .send(EngineFrame::RequestFailed {
                    id: id.to_string(),
                    error: EngineError::protocol(format!("row batch too large to stream: {e}")),
                })
                .await;
            if let Some(hook) = cancel_slot.lock().expect("cancel slot poisoned").take() {
                hook();
            }
            false
        }
    }
}
