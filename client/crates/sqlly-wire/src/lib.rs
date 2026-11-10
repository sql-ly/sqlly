//! `sqlly-wire`: relay outer-frame codec, chunking, and capacity math for the
//! SQLLY remote-engine transport.
//!
//! This crate is intentionally transport- and crypto-agnostic: it only encodes,
//! decodes, validates, splits, and reassembles opaque frame bodies. Encryption
//! lives in `sqlly-noise`; the engine payloads it carries live in
//! `sqlly-engine-protocol`.
//!
//! # Layers
//!
//! * [`RelayFrame`] — the fixed 60-byte, little-endian relay outer frame with
//!   an opaque [`bytes::Bytes`] body for zero-copy forwarding by the relay.
//! * [`split_into_chunks`] / [`ChunkAssembler`] — split a message into
//!   body-sized chunks and reassemble them at an endpoint (never on the relay).
//! * [`capacity_pct`] — the relay's `active`/`allowed` fullness percentage.
//!
//! The relay parses only the frame header and forwards the body untouched; it
//! never reassembles chunks or inspects payloads.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod capacity;
mod chunk;
mod codec;
mod error;
mod identity;

pub use capacity::capacity_pct;
pub use chunk::{
    AssemblerConfig, ChunkAssembler, MAX_CHUNK_COUNT, MAX_MESSAGE_SIZE_DEFAULT, split_into_chunks,
};
pub use codec::{
    FLAG_LAST_CHUNK, FRAME_LEN_PREFIX, FrameKind, HEADER_LEN, KNOWN_FLAGS, MAX_BODY_LEN,
    MAX_FRAME_LEN, MIN_FRAME_LEN, PROTOCOL_VERSION, RelayErrorCode, RelayFrame,
};
pub use error::{AssembleError, ChunkError, TimedOutMessage, WireError};
pub use identity::{DEVICE_ID_NAMESPACE, device_uuid};
