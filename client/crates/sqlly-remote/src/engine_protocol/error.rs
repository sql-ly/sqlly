//! Error type shared by the codec primitives.
//!
//! [`ProtocolError`] implements [`std::error::Error`] (via `thiserror`) so it
//! composes transparently with `anyhow` in `sqlly-cli`: `?` on a reader method
//! that returns `Result<_, ProtocolError>` converts into `anyhow::Error`, and
//! `anyhow::Context` is still available on the same `Result`. The `Display`
//! text of [`ProtocolError::Decode`] is the raw message string, so callers that
//! assert on substrings (e.g. `"truncated"`, `"invalid bool"`) keep working
//! exactly as they did when the cursor lived in `apps/cli/src/engine.rs`.

use std::io;

/// Errors produced while framing, reading, or writing engine wire bytes.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// A binary decode failed. The message is self-describing (names the field
    /// and offset) and its `Display` is exactly this string.
    #[error("{0}")]
    Decode(String),

    /// A frame's declared/actual length exceeds [`crate::engine_protocol::MAX_FRAME_BYTES`].
    #[error("engine frame too large: {0} bytes")]
    FrameTooLarge(usize),

    /// A frame payload was empty where at least a tag byte was required.
    #[error("engine frame payload is empty (missing tag byte)")]
    EmptyFrame,

    /// Underlying transport I/O failed.
    #[error("engine frame I/O error: {0}")]
    Io(#[from] io::Error),
}

impl ProtocolError {
    /// Construct a [`ProtocolError::Decode`] from anything `Display`-able.
    pub fn decode(message: impl Into<String>) -> Self {
        ProtocolError::Decode(message.into())
    }
}

/// Convenience result alias for codec operations.
pub type ProtocolResult<T> = Result<T, ProtocolError>;
