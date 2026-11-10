use super::*;

/// Soft target for rows-batch frame body size. Row batches are split before
/// sending so individual frames stay well under `MAX_FRAME_BYTES`, reducing
/// head-of-line blocking and preventing writer death from oversized frames.
pub(crate) const TARGET_ROWS_FRAME_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Encode an [`EngineFrame`] into a length-prefixed wire frame.
///
/// Wire layout: `[u32 LE total_len][u8 tag][body]`, where `total_len` counts
/// the tag byte plus the body. Simple frames carry hand-rolled binary bodies;
/// complex frames carry a string id followed by a length-prefixed JSON payload
/// for the nested type (same pattern as `connection_json` in requests).
pub(crate) fn encode_frame(frame: &EngineFrame) -> Result<Vec<u8>> {
    let (tag, body) = match frame {
        EngineFrame::ValidationReport {
            id,
            diagnostics,
            resolved,
        } => (
            FRAME_TAG_VALIDATION,
            encode_validation_body(id, diagnostics, resolved)?,
        ),
        EngineFrame::RowsBatch {
            id,
            result_set_index,
            rows,
        } => (
            FRAME_TAG_ROWS_BATCH,
            encode_rows_batch_body(id, *result_set_index, rows)?,
        ),
        EngineFrame::TruncatedCells {
            id,
            result_set_index,
            cells,
        } => (
            sqlly_remote::engine_protocol::FRAME_TAG_TRUNCATED_CELLS,
            encode_truncated_cells_body(id, *result_set_index, cells),
        ),
        EngineFrame::Ready { protocol_version } => {
            (FRAME_TAG_READY, encode_ready_body(*protocol_version)?)
        }
        EngineFrame::Pong { id } => (FRAME_TAG_PONG, encode_id_only_body(id)?),
        EngineFrame::RequestStarted { id } => (FRAME_TAG_REQUEST_STARTED, encode_id_only_body(id)?),
        EngineFrame::Event { id, event } => (FRAME_TAG_EVENT, encode_json_payload_body(id, event)?),
        EngineFrame::RequestCompleted { id } => {
            (FRAME_TAG_REQUEST_COMPLETED, encode_id_only_body(id)?)
        }
        // id + message + machine-readable code. The code is a trailing
        // addition old decoders never read (they stop after two strings), so
        // the frame stays compatible in both skew directions.
        EngineFrame::RequestFailed { id, error } => (
            FRAME_TAG_REQUEST_FAILED,
            Ok::<Vec<u8>, anyhow::Error>(
                BinaryBodyWriter::new()
                    .string(id)
                    .string(&error.to_string())
                    .string(error.code())
                    .done(),
            )?,
        ),
        EngineFrame::RequestCancelled { id, target_id } => (
            FRAME_TAG_REQUEST_CANCELLED,
            encode_two_string_body(id, target_id)?,
        ),
        EngineFrame::ShutdownAck { id } => (FRAME_TAG_SHUTDOWN_ACK, encode_id_only_body(id)?),
        EngineFrame::CatalogSyncReport { id, report } => (
            FRAME_TAG_CATALOG_SYNC_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::CatalogMaintenanceReport { id, report } => (
            FRAME_TAG_CATALOG_MAINTENANCE_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::ProviderStatusReport { id, report } => (
            FRAME_TAG_PROVIDER_STATUS_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::SqlReviewReport { id, report } => (
            FRAME_TAG_SQL_REVIEW_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::QuerySummaryReport { id, report } => (
            FRAME_TAG_QUERY_SUMMARY_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::RowEditReport { id, report } => (
            FRAME_TAG_ROW_EDIT_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::AiCacheBuildReport { id, report } => (
            FRAME_TAG_AI_CACHE_BUILD_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::InvalidateAiCacheReport { id, report } => (
            FRAME_TAG_INVALIDATE_AI_CACHE_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::SchemaAnswer { id, response } => (
            FRAME_TAG_SCHEMA_ANSWER,
            encode_json_payload_body(id, response)?,
        ),
        EngineFrame::SchemaSearchResults { id, results } => (
            FRAME_TAG_SCHEMA_SEARCH_RESULTS,
            encode_json_payload_body(id, results)?,
        ),
        EngineFrame::JoinPathReport { id, paths } => (
            FRAME_TAG_JOIN_PATH_REPORT,
            encode_json_payload_body(id, paths)?,
        ),
        EngineFrame::EmbeddingsBuildReport { id, report, note } => (
            FRAME_TAG_EMBEDDINGS_BUILD_REPORT,
            encode_json_payload_with_note_body(id, report, note.as_deref())?,
        ),
        EngineFrame::SqlGenerationReport { id, report } => (
            FRAME_TAG_SQL_GENERATION_REPORT,
            encode_json_payload_body(id, report)?,
        ),
        EngineFrame::SqlValidationReport { id, report } => (
            FRAME_TAG_SQL_VALIDATION_REPORT,
            encode_json_payload_body(id, report)?,
        ),
    };

    let total_len = body.len() + 1; // tag byte + body
    if total_len > MAX_FRAME_BYTES as usize {
        anyhow::bail!("engine frame too large: {} bytes", total_len);
    }
    let total_len_u32 = u32::try_from(total_len).context("frame total length exceeds u32")?;
    let mut bytes = Vec::with_capacity(total_len + 4);
    bytes.extend_from_slice(&total_len_u32.to_le_bytes());
    bytes.push(tag);
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

/// Flat binary body for a validation report (everything after the frame tag):
/// `[u32 id_len][id utf8][u32 diag_count]` then, per diagnostic,
/// `[u8 severity][u8 class][u32 span_start][u32 span_len][u32 line][u32 col][u32 msg_len][msg utf8]`,
/// followed by a resolved-spans section: `[u32 resolved_count]` then, per span,
/// `[u32 start][u32 len]`.
/// All integers little-endian; all offsets/lengths are UTF-8 bytes.
pub(crate) fn encode_validation_body(
    id: &str,
    diagnostics: &[WireDiag],
    resolved: &[WireSpan],
) -> Result<Vec<u8>> {
    let mut w = BinaryBodyWriter::new()
        .string(id)
        .u32_le(u32::try_from(diagnostics.len()).context("diagnostics count exceeds u32")?);
    for diag in diagnostics {
        w = w
            .u8(diag.severity)
            .u8(diag.class)
            .u32_le(diag.span_start)
            .u32_le(diag.span_len)
            .u32_le(diag.line)
            .u32_le(diag.col)
            .string(&diag.message);
    }
    w = w.u32_le(u32::try_from(resolved.len()).context("resolved count exceeds u32")?);
    for span in resolved {
        w = w.u32_le(span.start).u32_le(span.len);
    }
    Ok(w.done())
}

/// Body for responses that carry only an id: `[u32 id_len][id utf8]`.
pub(crate) fn encode_id_only_body(id: &str) -> Result<Vec<u8>> {
    Ok(BinaryBodyWriter::new().string(id).done())
}

/// Body for responses that carry two strings: `[u32 id_len][id][u32 str_len][str]`.
pub(crate) fn encode_two_string_body(id: &str, second: &str) -> Result<Vec<u8>> {
    Ok(BinaryBodyWriter::new().string(id).string(second).done())
}

/// Body for the Ready frame: `[u32 protocol_version]` (no id — Ready is the
/// first frame, before any request id exists).
/// `Ready` body: the legacy `[u32 protocol_version]` followed by the
/// capability list this engine advertises ([`ENGINE_CAPABILITIES`]). Clients
/// that predate negotiation read only the `u32`.
pub(crate) fn encode_ready_body(protocol_version: u32) -> Result<Vec<u8>> {
    sqlly_remote::engine_protocol::encode_ready_body(protocol_version, ENGINE_CAPABILITIES)
        .context("encode ready capabilities")
}

/// Body for responses that carry an id + a length-prefixed JSON payload:
/// `[u32 id_len][id utf8][u32 json_len][json utf8]`.
pub(crate) fn encode_json_payload_body<T: Serialize>(id: &str, payload: &T) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(payload).context("serialize response payload")?;
    let json_len = u32::try_from(json.len()).context("json payload length exceeds u32")?;
    Ok(BinaryBodyWriter::new()
        .string(id)
        .u32_le(json_len)
        .bytes(&json)
        .done())
}

/// Body for responses that carry an id + a length-prefixed JSON payload + an
/// optional string note:
/// `[u32 id_len][id utf8][u32 json_len][json utf8][u8 note_present][u32 note_len][note utf8]`.
pub(crate) fn encode_json_payload_with_note_body<T: Serialize>(
    id: &str,
    payload: &T,
    note: Option<&str>,
) -> Result<Vec<u8>> {
    let body = encode_json_payload_body(id, payload)?;
    Ok(BinaryBodyWriter::default()
        .bytes(&body)
        .opt_string(note)
        .done())
}

/// Estimates the encoded payload length for a single cell (excluding the
/// 1-byte tag). Used by [`rows_batch_body_len`] for pre-sizing and splitting.
pub(crate) fn encoded_cell_payload_len(cell: &CellValue) -> Result<usize> {
    let len = match cell {
        CellValue::Null => 0,
        CellValue::Bool(_) => 1,
        CellValue::I64(_) => 8,
        CellValue::F64(_) => 8,
        CellValue::String(s) => {
            let blen = s.len();
            u32::try_from(blen).context("string cell length exceeds u32")?;
            4 + blen
        }
        CellValue::Bytes(b) => {
            u32::try_from(b.len()).context("bytes cell length exceeds u32")?;
            4 + b.len()
        }
    };
    Ok(len)
}

/// Estimates the exact encoded body length for a rows-batch frame. Used for
/// buffer pre-sizing and byte-aware splitting. All arithmetic is checked to
/// catch overflow before allocation.
pub(crate) fn rows_batch_body_len(
    id: &str,
    result_set_index: usize,
    rows: &[Vec<CellValue>],
) -> Result<usize> {
    let id_len = id.len();
    u32::try_from(id_len).context("rows_batch id length exceeds u32")?;
    u32::try_from(result_set_index).context("result_set_index exceeds u32")?;
    u32::try_from(rows.len()).context("row count exceeds u32")?;

    let mut len = 4usize
        .checked_add(id_len)
        .and_then(|v| v.checked_add(4))
        .and_then(|v| v.checked_add(4))
        .context("rows_batch header size overflow")?;

    for row in rows {
        u32::try_from(row.len()).context("cell count exceeds u32")?;
        len = len.checked_add(4).context("row header size overflow")?;
        for cell in row {
            len = len.checked_add(1).context("cell tag size overflow")?;
            len = len
                .checked_add(encoded_cell_payload_len(cell)?)
                .context("cell payload size overflow")?;
        }
    }

    Ok(len)
}

/// Splits a row batch into chunks whose encoded body stays at or below
/// `TARGET_ROWS_FRAME_BODY_BYTES`. Each chunk preserves the same `id` and
/// `result_set_index`, and row order is maintained across chunks. If a single
/// row exceeds the target but fits within `MAX_FRAME_BYTES`, it is sent alone
/// in its own frame. If a single row cannot fit in any frame, an error is
/// returned so the caller can emit a `request_failed`.
pub(crate) fn split_rows_batch<'a>(
    id: &str,
    result_set_index: usize,
    rows: &'a [Vec<CellValue>],
) -> Result<Vec<&'a [Vec<CellValue>]>> {
    if rows.is_empty() {
        return Ok(vec![rows]);
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < rows.len() {
        let mut end = start;
        let mut last_good_end = None;

        while end < rows.len() {
            let candidate = &rows[start..=end];
            let len = rows_batch_body_len(id, result_set_index, candidate)?;
            if len > TARGET_ROWS_FRAME_BODY_BYTES {
                break;
            }
            last_good_end = Some(end + 1);
            end += 1;
        }

        let chunk_end = match last_good_end {
            Some(value) => value,
            None => {
                // Even a single row exceeds the target. Check if it can fit
                // in a frame at all (body + 1 tag byte <= MAX_FRAME_BYTES).
                // Cells are cut to 8 KiB previews in the executor drain
                // (`sqlly_services::query::large_cells`), so this branch is
                // only reachable for extremely wide rows; it stays as the
                // protocol's hard guarantee.
                let single_len =
                    rows_batch_body_len(id, result_set_index, &rows[start..start + 1])?;
                if single_len + 1 > MAX_FRAME_BYTES as usize {
                    anyhow::bail!(
                        "single row encoded body ({} bytes) exceeds MAX_FRAME_BYTES ({} bytes)",
                        single_len,
                        MAX_FRAME_BYTES
                    );
                }
                start + 1
            }
        };

        chunks.push(&rows[start..chunk_end]);
        start = chunk_end;
    }

    Ok(chunks)
}

/// The `truncated_cells` body, laid out by
/// `sqlly_remote::engine_protocol::layouts::encode_truncated_cells`.
pub(crate) fn encode_truncated_cells_body(
    id: &str,
    result_set_index: usize,
    cells: &[sqlly_services::TruncatedCell],
) -> Vec<u8> {
    use sqlly_remote::engine_protocol::layouts::{
        TRUNCATED_KIND_BYTES, TRUNCATED_KIND_TEXT, TruncatedCellWire, encode_truncated_cells,
    };
    let wire: Vec<TruncatedCellWire> = cells
        .iter()
        .map(|cell| TruncatedCellWire {
            row: cell.row as u64,
            column: cell.column as u32,
            total_len: cell.total_len,
            kind: match cell.kind {
                sqlly_services::TruncatedKind::Text => TRUNCATED_KIND_TEXT,
                sqlly_services::TruncatedKind::Bytes => TRUNCATED_KIND_BYTES,
            },
        })
        .collect();
    encode_truncated_cells(id, result_set_index as u32, &wire)
}

/// Flat binary body for a rows-batch frame (everything after the frame tag):
/// `[u32 id_len][id utf8][u32 result_set_index][u32 row_count]` then, per row,
/// `[u32 cell_count]` then, per cell, `[u8 cell_tag]` followed by a tag-specific
/// payload. All integers little-endian; all lengths are byte counts.
///
/// Cell tags:
/// - `0x00` Null — no payload
/// - `0x01` Bool — `u8` (0 or 1)
/// - `0x02` I64 — `i64` little-endian
/// - `0x03` F64 — `f64` little-endian
/// - `0x04` String — `u32 byte_len` + UTF-8 bytes
/// - `0x05` Bytes — `u32 byte_len` + raw bytes
pub(crate) fn encode_rows_batch_body(
    id: &str,
    result_set_index: usize,
    rows: &[Vec<CellValue>],
) -> Result<Vec<u8>> {
    let estimated = rows_batch_body_len(id, result_set_index, rows)?;
    let mut body = Vec::with_capacity(estimated);
    let id_bytes = id.as_bytes();
    let id_len = u32::try_from(id_bytes.len()).context("rows_batch id length exceeds u32")?;
    body.extend_from_slice(&id_len.to_le_bytes());
    body.extend_from_slice(id_bytes);
    let rs_idx = u32::try_from(result_set_index).context("result_set_index exceeds u32")?;
    body.extend_from_slice(&rs_idx.to_le_bytes());
    let row_count = u32::try_from(rows.len()).context("row count exceeds u32")?;
    body.extend_from_slice(&row_count.to_le_bytes());
    for row in rows {
        let cell_count = u32::try_from(row.len()).context("cell count exceeds u32")?;
        body.extend_from_slice(&cell_count.to_le_bytes());
        for cell in row {
            match cell {
                CellValue::Null => {
                    body.push(0x00);
                }
                CellValue::Bool(value) => {
                    body.push(0x01);
                    body.push(if *value { 1 } else { 0 });
                }
                CellValue::I64(value) => {
                    body.push(0x02);
                    body.extend_from_slice(&value.to_le_bytes());
                }
                CellValue::F64(value) => {
                    body.push(0x03);
                    body.extend_from_slice(&value.to_le_bytes());
                }
                CellValue::String(value) => {
                    body.push(0x04);
                    let s_bytes = value.as_bytes();
                    let s_len =
                        u32::try_from(s_bytes.len()).context("string cell length exceeds u32")?;
                    body.extend_from_slice(&s_len.to_le_bytes());
                    body.extend_from_slice(s_bytes);
                }
                CellValue::Bytes(value) => {
                    body.push(0x05);
                    let b_len =
                        u32::try_from(value.len()).context("bytes cell length exceeds u32")?;
                    body.extend_from_slice(&b_len.to_le_bytes());
                    body.extend_from_slice(value);
                }
            }
        }
    }
    Ok(body)
}
