//! Length-prefixed frame framing shared by the CLI engine, the bridge, GPUI
//! remote sessions, and tests.
//!
//! Wire envelope: `[u32 LE total_len][u8 tag][body]`, where `total_len` counts
//! the tag byte plus the body. This is exactly the framing produced by
//! `encode_frame` in `apps/cli/src/engine.rs`; the byte layout is unchanged.
//!
//! A *frame payload* in this module is the `[u8 tag][body]` slice — i.e. the
//! bytes that remain after the `u32` length prefix has been consumed. The
//! `read_length_prefixed*` helpers return that payload; [`RawFrame`] splits it
//! into `tag` + `body`.

#[cfg(not(target_arch = "wasm32"))]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::engine_protocol::constants::{FRAME_TAG_READY, MAX_FRAME_BYTES, is_terminal_frame_tag};
use crate::engine_protocol::error::{ProtocolError, ProtocolResult};

/// A decoded frame envelope: a discriminator `tag` plus its opaque `body`.
///
/// The `body` for every tagged frame except `Ready` begins with a
/// length-prefixed id (`[u32 id_len][id utf8]`), which [`RawFrame::id`] parses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    pub tag: u8,
    pub body: Vec<u8>,
}

impl RawFrame {
    pub fn new(tag: u8, body: Vec<u8>) -> Self {
        Self { tag, body }
    }

    /// Split a frame payload (`[tag][body]`) into a [`RawFrame`]. Errors when
    /// the payload is empty (no tag byte).
    pub fn from_payload(payload: &[u8]) -> ProtocolResult<Self> {
        let (&tag, body) = payload.split_first().ok_or(ProtocolError::EmptyFrame)?;
        Ok(Self {
            tag,
            body: body.to_vec(),
        })
    }

    /// The frame payload `[tag][body]`, without the outer length prefix.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.body.len());
        out.push(self.tag);
        out.extend_from_slice(&self.body);
        out
    }

    /// The complete wire bytes `[u32 total_len][tag][body]`.
    pub fn encode(&self) -> ProtocolResult<Vec<u8>> {
        encode_envelope(self.tag, &self.body)
    }

    /// Whether this frame is one of the three terminal response tags.
    pub fn is_terminal(&self) -> bool {
        is_terminal_frame_tag(self.tag)
    }

    /// Parse the leading request id from the body, or `None` for a `Ready`
    /// frame (whose body is the protocol version, not an id) or a malformed
    /// body.
    pub fn id(&self) -> Option<String> {
        frame_id(self.tag, &self.body)
    }
}

/// Encode `[u32 LE total_len][u8 tag][body]`. Rejects bodies that would push the
/// frame past [`MAX_FRAME_BYTES`].
pub fn encode_envelope(tag: u8, body: &[u8]) -> ProtocolResult<Vec<u8>> {
    let total_len = body.len() + 1; // tag byte + body
    if total_len > MAX_FRAME_BYTES as usize {
        return Err(ProtocolError::FrameTooLarge(total_len));
    }
    let total_len_u32 =
        u32::try_from(total_len).map_err(|_| ProtocolError::FrameTooLarge(total_len))?;
    let mut bytes = Vec::with_capacity(total_len + 4);
    bytes.extend_from_slice(&total_len_u32.to_le_bytes());
    bytes.push(tag);
    bytes.extend_from_slice(body);
    Ok(bytes)
}

/// Encode a length-prefixed id: `[u32 id_len][id utf8]`. Shared prefix of every
/// tagged response body.
pub fn encode_id_prefix(id: &str) -> ProtocolResult<Vec<u8>> {
    let id_bytes = id.as_bytes();
    let id_len = u32::try_from(id_bytes.len())
        .map_err(|_| ProtocolError::decode("id length exceeds u32"))?;
    let mut body = Vec::with_capacity(4 + id_bytes.len());
    body.extend_from_slice(&id_len.to_le_bytes());
    body.extend_from_slice(id_bytes);
    Ok(body)
}

/// Read the leading `[u32 id_len][id utf8]` from a frame body. Returns `None`
/// for a `Ready` frame or a truncated/invalid body.
pub fn frame_id(tag: u8, body: &[u8]) -> Option<String> {
    if tag == FRAME_TAG_READY {
        return None;
    }
    let (first_end, first) = read_body_string(body, 0)?;
    // A `request_cancelled` frame belongs to the request it CANCELLED, not to
    // the cancel request that triggered it: its body is
    // `[cancel_id][target_id]` and the target's response stream is the one it
    // terminates. Routing by the first string would deliver it to a channel
    // nobody listens on, leaving the cancelled request's stream open forever.
    if tag == crate::engine_protocol::constants::FRAME_TAG_REQUEST_CANCELLED
        && let Some((_, target)) = read_body_string(body, first_end)
    {
        return Some(target);
    }
    Some(first)
}

/// Read one `[u32 len][utf8]` string at `offset`, returning the end offset
/// and the string.
fn read_body_string(body: &[u8], offset: usize) -> Option<(usize, String)> {
    let len_end = offset.checked_add(4)?;
    if body.len() < len_end {
        return None;
    }
    let len = u32::from_le_bytes([
        body[offset],
        body[offset + 1],
        body[offset + 2],
        body[offset + 3],
    ]) as usize;
    let end = len_end.checked_add(len)?;
    if end > body.len() {
        return None;
    }
    std::str::from_utf8(&body[len_end..end])
        .ok()
        .map(|s| (end, s.to_string()))
}

/// Read one length-prefixed frame payload (`[tag][body]`) from an async reader.
#[cfg(not(target_arch = "wasm32"))]
///
/// Returns `Ok(None)` on a clean EOF *before any byte of the next frame*
/// (matching the engine's `read_frame` behavior), and errors on a partial
/// frame, an oversized length, or other I/O failure.
pub async fn read_length_prefixed<R>(input: &mut R) -> ProtocolResult<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(ProtocolError::Io(error)),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len as usize));
    }
    let mut payload = vec![0u8; len as usize];
    input.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

#[cfg(not(target_arch = "wasm32"))]
/// Write one length-prefixed frame payload (`[tag][body]`) to an async writer
/// and flush it. `payload` must already be the `[tag][body]` slice.
pub async fn write_length_prefixed<W>(output: &mut W, payload: &[u8]) -> ProtocolResult<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_BYTES as usize {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let len = payload.len() as u32;
    output.write_all(&len.to_le_bytes()).await?;
    output.write_all(payload).await?;
    output.flush().await?;
    Ok(())
}

/// Blocking counterpart of [`read_length_prefixed`] for synchronous stdio
/// readers (the engine's stdin thread, the subprocess test harness). Returns
/// `Ok(None)` on EOF before the next frame begins.
pub fn read_length_prefixed_blocking<R>(input: &mut R) -> ProtocolResult<Option<Vec<u8>>>
where
    R: std::io::Read,
{
    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(ProtocolError::Io(error)),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len as usize));
    }
    let mut payload = vec![0u8; len as usize];
    input.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// Blocking counterpart of [`write_length_prefixed`]. Writes the length prefix
/// and payload and flushes.
pub fn write_length_prefixed_blocking<W>(output: &mut W, payload: &[u8]) -> ProtocolResult<()>
where
    W: std::io::Write,
{
    if payload.len() > MAX_FRAME_BYTES as usize {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let len = payload.len() as u32;
    output.write_all(&len.to_le_bytes())?;
    output.write_all(payload)?;
    output.flush()?;
    Ok(())
}
