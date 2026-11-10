//! Per-chunk transport encryption with authenticated inner metadata.
//!
//! Each chunk is sealed as its own Noise transport message. Before encryption
//! we prepend an authenticated inner header duplicating the relay's clear outer
//! routing/chunk metadata:
//!
//! ```text
//! offset  size  field
//! 0       16    source Uuid (big-endian bytes)
//! 16      16    destination Uuid (big-endian bytes)
//! 32      8     message_id (u64 LE)
//! 40      4     chunk_index (u32 LE)
//! 44      1     flags (u8)
//! 45      ..    payload
//! ```
//!
//! On open, the receiver parses this header and compares it against the outer
//! frame it arrived in ([`OuterMeta`]); a mismatch means a relay tried to
//! rewrite metadata and is rejected.

use bytes::Bytes;
use uuid::Uuid;

use crate::noise::{NoiseError, Result};

/// Fixed size of the authenticated inner header preceding the payload.
pub const INNER_HEADER_LEN: usize = 16 + 16 + 8 + 4 + 1;

/// Routing/chunk metadata carried alongside a chunk. This mirrors the clear
/// fields of a relay outer frame; the sealed copy is authenticated so the relay
/// cannot rewrite it undetected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerChunk {
    pub source: Uuid,
    pub destination: Uuid,
    pub message_id: u64,
    pub chunk_index: u32,
    pub flags: u8,
    pub payload: Bytes,
}

/// The clear outer frame header a decrypted [`InnerChunk`] is checked against.
/// (`sqlly-noise` stays agnostic of `sqlly-wire`; the caller populates this
/// from whatever frame type it uses.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OuterMeta {
    pub source: Uuid,
    pub destination: Uuid,
    pub message_id: u64,
    pub chunk_index: u32,
    pub flags: u8,
}

impl InnerChunk {
    /// Serialize header + payload into the plaintext that gets encrypted.
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(INNER_HEADER_LEN + self.payload.len());
        buf.extend_from_slice(self.source.as_bytes());
        buf.extend_from_slice(self.destination.as_bytes());
        buf.extend_from_slice(&self.message_id.to_le_bytes());
        buf.extend_from_slice(&self.chunk_index.to_le_bytes());
        buf.push(self.flags);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Parse decrypted plaintext back into an [`InnerChunk`].
    fn decode(plaintext: &[u8]) -> Result<Self> {
        if plaintext.len() < INNER_HEADER_LEN {
            return Err(NoiseError::ChunkTooShort);
        }
        let source = Uuid::from_slice(&plaintext[0..16]).expect("16 bytes");
        let destination = Uuid::from_slice(&plaintext[16..32]).expect("16 bytes");
        let message_id = u64::from_le_bytes(plaintext[32..40].try_into().expect("8 bytes"));
        let chunk_index = u32::from_le_bytes(plaintext[40..44].try_into().expect("4 bytes"));
        let flags = plaintext[44];
        let payload = Bytes::copy_from_slice(&plaintext[INNER_HEADER_LEN..]);
        Ok(Self {
            source,
            destination,
            message_id,
            chunk_index,
            flags,
            payload,
        })
    }

    /// Whether this chunk's authenticated metadata agrees with a clear outer
    /// frame header.
    pub fn matches_outer(&self, outer: &OuterMeta) -> bool {
        self.source == outer.source
            && self.destination == outer.destination
            && self.message_id == outer.message_id
            && self.chunk_index == outer.chunk_index
            && self.flags == outer.flags
    }
}

/// Established Noise transport cipher (post-handshake). Full-duplex: `seal`
/// uses the sending cipher, `open` the receiving cipher. Both are nonce-ordered
/// streams, so the two endpoints must process chunks in matching order.
///
/// Not `Debug`-derived: wraps `snow` transport state holding session keys.
pub struct Transport {
    state: snow::TransportState,
}

/// Redacted: never prints session-key material.
impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transport { <noise session redacted> }")
    }
}

impl Transport {
    pub(crate) fn new(state: snow::TransportState) -> Self {
        Self { state }
    }

    /// Encrypt one chunk (header + payload) as an independent AEAD message.
    pub fn seal(&mut self, inner: &InnerChunk) -> Result<Vec<u8>> {
        let plaintext = inner.encode();
        // ChaChaPoly adds a 16-byte tag.
        let mut buf = vec![0u8; plaintext.len() + 16];
        let n = self.state.write_message(&plaintext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Decrypt one chunk, returning the authenticated inner metadata + payload.
    /// The caller compares it to the clear outer frame (see
    /// [`Self::open_verified`]).
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<InnerChunk> {
        let mut buf = vec![0u8; ciphertext.len()];
        let n = self.state.read_message(ciphertext, &mut buf)?;
        buf.truncate(n);
        InnerChunk::decode(&buf)
    }

    /// Decrypt one chunk and verify its authenticated metadata against the
    /// clear outer frame header, returning just the payload on success.
    /// Rejects with [`NoiseError::MetadataMismatch`] if they disagree.
    pub fn open_verified(&mut self, ciphertext: &[u8], outer: &OuterMeta) -> Result<Bytes> {
        let inner = self.open(ciphertext)?;
        if !inner.matches_outer(outer) {
            return Err(NoiseError::MetadataMismatch);
        }
        Ok(inner.payload)
    }
}
