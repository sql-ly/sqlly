//! Wire constants shared by every SQLLY engine speaker.
//!
//! The byte values are load-bearing: every engine speaker (the CLI engine,
//! the GPUI app, the remote bridge/relay stack) encodes against them, so they
//! must never change without a coordinated protocol bump.

/// Engine protocol version announced in the `Ready` frame.
pub const PROTOCOL_VERSION: u32 = 2;

/// Hard ceiling on a single length-prefixed frame body. Reads and writes reject
/// anything larger to bound per-frame allocation.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Layout version stamped into every binary *request* body. Separate from
/// [`PROTOCOL_VERSION`] so individual body layouts can evolve without a full
/// protocol cutover. Decoders reject mismatched versions gracefully.
///
/// v2: the `sync_catalog` object layout gained a `primary_key` string list
/// (after `columns`, before `foreign_keys`).
pub const BINARY_REQUEST_LAYOUT_VERSION: u16 = 2;

// ---------------------------------------------------------------------------
// Response frame discriminator tags. Every response body is prefixed with one
// of these so the reader can branch before parsing.
// ---------------------------------------------------------------------------

pub const FRAME_TAG_VALIDATION: u8 = 0x01;
pub const FRAME_TAG_ROWS_BATCH: u8 = 0x02;
pub const FRAME_TAG_READY: u8 = 0x03;
pub const FRAME_TAG_PONG: u8 = 0x04;
pub const FRAME_TAG_REQUEST_STARTED: u8 = 0x05;
pub const FRAME_TAG_EVENT: u8 = 0x06;
pub const FRAME_TAG_REQUEST_COMPLETED: u8 = 0x07;
pub const FRAME_TAG_REQUEST_FAILED: u8 = 0x08;
pub const FRAME_TAG_REQUEST_CANCELLED: u8 = 0x09;
pub const FRAME_TAG_SHUTDOWN_ACK: u8 = 0x0A;
pub const FRAME_TAG_CATALOG_SYNC_REPORT: u8 = 0x0B;
pub const FRAME_TAG_AI_CACHE_BUILD_REPORT: u8 = 0x0C;
pub const FRAME_TAG_INVALIDATE_AI_CACHE_REPORT: u8 = 0x0D;
pub const FRAME_TAG_SCHEMA_ANSWER: u8 = 0x0E;
pub const FRAME_TAG_SCHEMA_SEARCH_RESULTS: u8 = 0x0F;
pub const FRAME_TAG_JOIN_PATH_REPORT: u8 = 0x10;
pub const FRAME_TAG_EMBEDDINGS_BUILD_REPORT: u8 = 0x11;
pub const FRAME_TAG_SQL_GENERATION_REPORT: u8 = 0x12;
pub const FRAME_TAG_SQL_VALIDATION_REPORT: u8 = 0x13;
pub const FRAME_TAG_ROW_EDIT_REPORT: u8 = 0x14;
pub const FRAME_TAG_CATALOG_MAINTENANCE_REPORT: u8 = 0x15;
pub const FRAME_TAG_PROVIDER_STATUS_REPORT: u8 = 0x16;
pub const FRAME_TAG_SQL_REVIEW_REPORT: u8 = 0x17;
pub const FRAME_TAG_QUERY_SUMMARY_REPORT: u8 = 0x18;
/// Cells of the preceding `rows_batch` frames that were cut to previews
/// (sent only for an `execute_query` that asked for
/// [`EXECUTE_QUERY_OPTION_CELL_PREVIEWS`]). Not terminal.
pub const FRAME_TAG_TRUNCATED_CELLS: u8 = 0x19;

/// `execute_query` option bit: cut oversized cells to previews and report
/// them (see `CAPABILITY_QUERY_CELL_PREVIEWS`).
pub const EXECUTE_QUERY_OPTION_CELL_PREVIEWS: u32 = 1;

// ---------------------------------------------------------------------------
// Inbound request frame tags. Every request body is prefixed with one of these.
// ---------------------------------------------------------------------------

pub const REQUEST_TAG_PING: u8 = 0x01;
pub const REQUEST_TAG_SHUTDOWN: u8 = 0x02;
pub const REQUEST_TAG_CANCEL: u8 = 0x03;
pub const REQUEST_TAG_EXECUTE_QUERY: u8 = 0x04;
pub const REQUEST_TAG_BUILD_AI_CACHE: u8 = 0x05;
pub const REQUEST_TAG_INVALIDATE_AI_CACHE: u8 = 0x06;
pub const REQUEST_TAG_ASK_SCHEMA: u8 = 0x07;
pub const REQUEST_TAG_SEARCH_SCHEMA: u8 = 0x08;
pub const REQUEST_TAG_FIND_JOIN_PATH: u8 = 0x09;
pub const REQUEST_TAG_BUILD_EMBEDDINGS: u8 = 0x0A;
pub const REQUEST_TAG_GENERATE_SQL: u8 = 0x0B;
pub const REQUEST_TAG_VALIDATE_SQL: u8 = 0x0C;
pub const REQUEST_TAG_VALIDATE: u8 = 0x10;
pub const REQUEST_TAG_SYNC_CATALOG: u8 = 0x11;
pub const REQUEST_TAG_ROW_EDIT: u8 = 0x12;
pub const REQUEST_TAG_CATALOG_MAINTENANCE: u8 = 0x13;
pub const REQUEST_TAG_PROVIDER_STATUS: u8 = 0x14;
pub const REQUEST_TAG_REVIEW_SQL: u8 = 0x15;
pub const REQUEST_TAG_SUMMARIZE_QUERY: u8 = 0x16;

/// Recognizes the three *terminal* response tags: once a response with one of
/// these tags is seen for a request id, that request's stream is complete and
/// no further frames will arrive for it. Ported from the `request_completed` /
/// `request_failed` / `request_cancelled` recognition in `engine.rs`.
///
/// `Pong` and `ShutdownAck` are also single-shot answers but are recognized
/// separately by [`crate::engine_protocol::client`] because they are not part of the general
/// streaming-request lifecycle.
pub fn is_terminal_frame_tag(tag: u8) -> bool {
    matches!(
        tag,
        FRAME_TAG_REQUEST_COMPLETED | FRAME_TAG_REQUEST_FAILED | FRAME_TAG_REQUEST_CANCELLED
    )
}
