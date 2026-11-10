//! A framed-stream layer that reads/writes [`RelayFrame`]s over any
//! `AsyncRead + AsyncWrite` byte stream.
//!
//! Reads are incremental: bytes accumulate in an internal buffer until a
//! complete frame (a 4-byte length prefix plus that many bytes) is present,
//! then exactly one frame is split off and decoded. This is deliberately
//! transport-agnostic so the session state machine can be unit-tested over
//! `tokio::io::duplex` with no socket or TLS.

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use sqlly_wire::{FRAME_LEN_PREFIX, MAX_FRAME_LEN, RelayFrame, WireError};

use crate::transport::error::{RemoteError, Result};

/// Reads and writes length-delimited [`RelayFrame`]s over a duplex byte stream.
pub struct FramedStream<S> {
    stream: S,
    read_buf: BytesMut,
}

impl<S> FramedStream<S> {
    /// Wrap a byte stream. No I/O happens until [`recv`](Self::recv) /
    /// [`send`](Self::send) are called.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(8 * 1024),
        }
    }

    /// Recover the underlying stream. Any buffered-but-unparsed bytes are
    /// discarded, so only call this at a frame boundary. After `authenticate`,
    /// hand the `FramedStream` itself to
    /// [`PeerSession::establish_framed`](crate::transport::PeerSession::establish_framed)
    /// instead: the relay may forward the peer's first handshake frame right
    /// behind the Auth ack, and one read can buffer both.
    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S> FramedStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Encode `frame` and write it (with a flush) to the stream.
    ///
    /// The relay body is opaque here — this layer never logs it.
    pub async fn send(&mut self, frame: &RelayFrame) -> Result<()> {
        let bytes = frame.encode()?;
        self.stream.write_all(&bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read the next complete [`RelayFrame`].
    ///
    /// * `Ok(Some(frame))` — one frame was decoded.
    /// * `Ok(None)` — the peer closed the stream cleanly at a frame boundary.
    /// * `Err(_)` — an I/O error, a structurally invalid frame, or EOF in the
    ///   middle of a frame.
    pub async fn recv(&mut self) -> Result<Option<RelayFrame>> {
        loop {
            // Do we already hold a full frame?
            if self.read_buf.len() >= FRAME_LEN_PREFIX {
                let frame_len = u32::from_le_bytes([
                    self.read_buf[0],
                    self.read_buf[1],
                    self.read_buf[2],
                    self.read_buf[3],
                ]) as usize;

                // Refuse an oversized declared length before we wait to buffer
                // it, so a hostile peer can't force an allocation.
                if frame_len > MAX_FRAME_LEN {
                    return Err(RemoteError::Wire(WireError::Oversized {
                        len: frame_len,
                        max: MAX_FRAME_LEN,
                    }));
                }

                let total = FRAME_LEN_PREFIX + frame_len;
                if self.read_buf.len() >= total {
                    let frame_bytes = self.read_buf.split_to(total).freeze();
                    return Ok(Some(RelayFrame::decode_exact(frame_bytes)?));
                }
            }

            // Otherwise pull more bytes.
            let n = self.stream.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                // Bytes remained mid-frame: a truncated frame.
                return Err(RemoteError::Wire(WireError::Truncated {
                    have: self.read_buf.len(),
                    need: FRAME_LEN_PREFIX,
                }));
            }
        }
    }
}
