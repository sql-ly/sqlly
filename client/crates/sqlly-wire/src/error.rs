//! Error types for relay frame encode/decode and chunk reassembly.
//!
//! Each enum variant carries a doc comment; the individual struct-field values
//! are named to match the messages in their `#[error(..)]` format strings, so
//! `missing_docs` is relaxed for those fields only.
#![allow(missing_docs)]

use thiserror::Error;

/// Errors produced while encoding, decoding, or validating a relay frame.
///
/// These never carry payload bytes, so they are safe to log.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The buffer held fewer than the minimum header bytes, or the declared
    /// frame length ran past the supplied buffer. Callers reading from a stream
    /// should treat [`crate::RelayFrame::decode`] returning `Ok(None)` as
    /// "need more bytes"; this variant is only surfaced by the whole-buffer
    /// decode helpers when the frame is structurally too small to be valid.
    #[error("frame is truncated: have {have} bytes, need at least {need}")]
    Truncated { have: usize, need: usize },

    /// The `protocol_version` field did not match a version this build speaks.
    #[error("unknown relay protocol version {0}")]
    UnknownVersion(u16),

    /// The `frame_kind` byte did not map to a known [`crate::FrameKind`].
    #[error("unknown relay frame kind {0}")]
    UnknownFrameKind(u8),

    /// One or more reserved flag bits were set.
    #[error("reserved flag bits set: {0:#010b}")]
    ReservedFlags(u8),

    /// The declared `frame_len` / `body_len` fields disagree with each other or
    /// with the actual body length.
    #[error("frame length mismatch: declared body {declared}, actual {actual}")]
    LengthMismatch { declared: usize, actual: usize },

    /// The frame (or its body) exceeded the maximum permitted size.
    #[error("frame body too large: {len} bytes exceeds max {max}")]
    Oversized { len: usize, max: usize },

    /// The `source_client_id` was the nil UUID, which is never a legal client.
    #[error("source client id is nil")]
    NilSourceId,

    /// The `destination_client_id` was the nil UUID, which is never a legal
    /// client.
    #[error("destination client id is nil")]
    NilDestinationId,
}

/// Errors produced while splitting a message into chunks.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ChunkError {
    /// `max_body` was zero, so no progress could ever be made.
    #[error("max chunk body must be non-zero")]
    ZeroMaxBody,

    /// The message would require more chunks than the protocol allows.
    #[error("message needs {needed} chunks, exceeds max {max}")]
    TooManyChunks { needed: u64, max: u32 },
}

/// Errors produced by [`crate::ChunkAssembler`] while reassembling a message.
///
/// These never carry payload bytes, so they are safe to log.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AssembleError {
    /// A chunk declared `chunk_count == 0`, which is never valid.
    #[error("chunk_count must be non-zero (message {message_id})")]
    ZeroChunkCount { message_id: u64 },

    /// A chunk's `chunk_count` exceeded the assembler's configured maximum.
    #[error("chunk_count {count} exceeds max {max} (message {message_id})")]
    ChunkCountTooLarge {
        message_id: u64,
        count: u32,
        max: u32,
    },

    /// The `chunk_index` was `>= chunk_count`.
    #[error("chunk index {index} out of range for count {count} (message {message_id})")]
    IndexOutOfRange {
        message_id: u64,
        index: u32,
        count: u32,
    },

    /// A later chunk reported a different `chunk_count` than an earlier chunk
    /// for the same message.
    #[error("inconsistent chunk_count: had {existing}, got {got} (message {message_id})")]
    InconsistentChunkCount {
        message_id: u64,
        existing: u32,
        got: u32,
    },

    /// The same chunk index was received twice for one message.
    #[error("duplicate chunk index {index} (message {message_id})")]
    DuplicateChunk { message_id: u64, index: u32 },

    /// Accepting this chunk would push the reconstructed message past the
    /// configured maximum size.
    #[error("reconstructed message would exceed max size {max} (message {message_id})")]
    MessageTooLarge { message_id: u64, max: usize },

    /// Starting a new message would exceed the maximum number of concurrently
    /// in-flight messages.
    #[error("too many in-flight messages (max {max})")]
    TooManyInFlight { max: usize },
}

/// A message that was dropped by [`crate::ChunkAssembler`] because it did not
/// complete before the inactivity timeout elapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOutMessage {
    /// The message id that timed out.
    pub message_id: u64,
    /// How many chunks had arrived before the timeout.
    pub received: u32,
    /// How many chunks the message expected in total.
    pub expected: u32,
}
