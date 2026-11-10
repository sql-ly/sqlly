//! Sealing and opening of engine-frame chunks.
//!
//! An engine frame (already the `[tag][body]` payload from
//! `sqlly-engine-protocol`) is split into body-sized chunks by `sqlly-wire`,
//! and each chunk is sealed independently with a `sqlly-noise` [`Transport`].
//!
//! Before sealing, the plaintext gets a 4-byte little-endian `chunk_count`
//! prefix. `sqlly-noise`'s [`InnerChunk`] already authenticates
//! `source`/`destination`/`message_id`/`chunk_index`/`flags`; binding
//! `chunk_count` inside the AEAD plaintext too closes the last metadata-integrity
//! gap, so a relay cannot rewrite how many chunks a message claims without
//! detection.

use bytes::{Bytes, BytesMut};

use crate::noise::{InnerChunk, OuterMeta, Transport};
use sqlly_wire::{FLAG_LAST_CHUNK, RelayFrame, split_into_chunks};
use uuid::Uuid;

use crate::transport::error::{RemoteError, Result};

/// Number of bytes the bound `chunk_count` prefix occupies in the sealed
/// plaintext.
const CHUNK_COUNT_PREFIX: usize = 4;

/// Split `message` into chunks, seal each with `transport`, and wrap each in a
/// [`FrameKind::Data`](sqlly_wire::FrameKind::Data) relay frame.
///
/// The frames must be sent in the returned order: each seal advances the Noise
/// sending nonce, so the peer must open them in the same order.
pub(crate) fn seal_engine_message(
    transport: &mut Transport,
    source: Uuid,
    destination: Uuid,
    message_id: u64,
    message: &[u8],
    max_chunk: usize,
) -> Result<Vec<RelayFrame>> {
    let chunks = split_into_chunks(message, max_chunk)?;
    let chunk_count = chunks.len() as u32;

    let mut frames = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.into_iter().enumerate() {
        let chunk_index = index as u32;
        let is_last = chunk_index + 1 == chunk_count;
        let flags = if is_last { FLAG_LAST_CHUNK } else { 0 };

        // Bind chunk_count inside the authenticated plaintext.
        let mut payload = BytesMut::with_capacity(CHUNK_COUNT_PREFIX + chunk.len());
        payload.extend_from_slice(&chunk_count.to_le_bytes());
        payload.extend_from_slice(&chunk);

        let inner = InnerChunk {
            source,
            destination,
            message_id,
            chunk_index,
            flags,
            payload: payload.freeze(),
        };
        let ciphertext = transport.seal(&inner)?;

        // RelayFrame::data derives the same FLAG_LAST_CHUNK from the indices,
        // so the clear outer flags match the sealed inner flags.
        frames.push(RelayFrame::data(
            source,
            destination,
            message_id,
            chunk_index,
            chunk_count,
            Bytes::from(ciphertext),
        ));
    }
    Ok(frames)
}

/// Open one [`FrameKind::Data`](sqlly_wire::FrameKind::Data) frame: decrypt,
/// verify the authenticated inner metadata against the clear outer header
/// (including the bound `chunk_count`), and return the chunk's plaintext bytes.
///
/// The caller feeds the returned bytes to a `ChunkAssembler` using the frame's
/// own `message_id` / `chunk_index` / `chunk_count`.
pub(crate) fn open_data_frame(transport: &mut Transport, frame: &RelayFrame) -> Result<Bytes> {
    let outer = OuterMeta {
        source: frame.source,
        destination: frame.destination,
        message_id: frame.message_id,
        chunk_index: frame.chunk_index,
        flags: frame.flags,
    };

    // Verifies source/destination/message_id/chunk_index/flags against the
    // outer header and authenticates the ciphertext.
    let plaintext = transport.open_verified(&frame.body, &outer)?;

    if plaintext.len() < CHUNK_COUNT_PREFIX {
        return Err(RemoteError::MalformedChunk);
    }
    let bound_count = u32::from_le_bytes([plaintext[0], plaintext[1], plaintext[2], plaintext[3]]);
    if bound_count != frame.chunk_count {
        return Err(RemoteError::ChunkCountMismatch {
            outer: frame.chunk_count,
            inner: bound_count,
        });
    }

    Ok(plaintext.slice(CHUNK_COUNT_PREFIX..))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::{Handshake, StaticKeypair};

    /// Run a full XX handshake in memory and return the two peer transports plus
    /// their ids, with no relay or async involved.
    fn transport_pair() -> (Transport, Transport, Uuid, Uuid) {
        let init_kp = StaticKeypair::generate().unwrap();
        let resp_kp = StaticKeypair::generate().unwrap();
        let init_pub = init_kp.public_key();
        let resp_pub = resp_kp.public_key();

        let mut ini = Handshake::initiator(&init_kp).unwrap();
        let mut res = Handshake::responder(&resp_kp).unwrap();

        let m1 = ini.write_message(&[]).unwrap();
        res.read_message(&m1).unwrap();
        let m2 = res.write_message(&[]).unwrap();
        ini.read_message(&m2).unwrap();
        let m3 = ini.write_message(&[]).unwrap();
        res.read_message(&m3).unwrap();

        let it = ini.into_transport_pinned(&resp_pub).unwrap();
        let rt = res.into_transport_pinned(&init_pub).unwrap();
        (it, rt, Uuid::from_u128(1), Uuid::from_u128(2))
    }

    #[test]
    fn seal_open_round_trip_single_and_multi_chunk() {
        let (mut it, mut rt, src, dst) = transport_pair();

        for msg in [b"x".as_slice(), b"hello engine frame payload".as_slice()] {
            let frames = seal_engine_message(&mut it, src, dst, 7, msg, 8).unwrap();
            let mut out = Vec::new();
            for f in &frames {
                out.extend_from_slice(&open_data_frame(&mut rt, f).unwrap());
            }
            assert_eq!(out, msg);
        }
    }

    /// A chunk of exactly [`MAX_CHUNK_PLAINTEXT_LIMIT`] bytes seals into one
    /// maximal 65,535-byte Noise message and opens; one byte more (at the same
    /// chunk size) spills into a second chunk and still round-trips.
    #[test]
    fn seal_at_exact_noise_limit_and_one_byte_over() {
        use crate::transport::session::MAX_CHUNK_PLAINTEXT_LIMIT;

        let (mut it, mut rt, src, dst) = transport_pair();

        let msg = vec![0xA5u8; MAX_CHUNK_PLAINTEXT_LIMIT];
        let frames =
            seal_engine_message(&mut it, src, dst, 1, &msg, MAX_CHUNK_PLAINTEXT_LIMIT).unwrap();
        assert_eq!(frames.len(), 1, "exactly-at-limit message is one chunk");
        assert_eq!(
            frames[0].body.len(),
            65_535,
            "ciphertext hits the Noise ceiling"
        );
        assert_eq!(open_data_frame(&mut rt, &frames[0]).unwrap(), msg);

        let msg = vec![0x5Au8; MAX_CHUNK_PLAINTEXT_LIMIT + 1];
        let frames =
            seal_engine_message(&mut it, src, dst, 2, &msg, MAX_CHUNK_PLAINTEXT_LIMIT).unwrap();
        assert_eq!(frames.len(), 2, "one byte over the limit needs two chunks");
        let mut out = Vec::new();
        for f in &frames {
            out.extend_from_slice(&open_data_frame(&mut rt, f).unwrap());
        }
        assert_eq!(out, msg);
    }

    /// A chunk size above the hard cap cannot be sealed at all: snow rejects
    /// the oversized transport message. This is the failure the SessionConfig
    /// clamp exists to prevent.
    #[test]
    fn chunk_above_noise_limit_fails_to_seal() {
        use crate::transport::session::MAX_CHUNK_PLAINTEXT_LIMIT;

        let (mut it, _rt, src, dst) = transport_pair();
        let over = MAX_CHUNK_PLAINTEXT_LIMIT + 1;
        let msg = vec![0u8; over];
        let err = seal_engine_message(&mut it, src, dst, 3, &msg, over);
        assert!(
            matches!(err, Err(RemoteError::Noise(_))),
            "sealing past the Noise ceiling must fail, got {err:?}"
        );
    }

    #[test]
    fn tamper_flips_a_sealed_byte() {
        let (mut it, mut rt, src, dst) = transport_pair();
        let mut frames = seal_engine_message(&mut it, src, dst, 1, b"payload", 64).unwrap();
        let mut body = frames[0].body.to_vec();
        body[0] ^= 0x01;
        frames[0].body = Bytes::from(body);
        assert!(matches!(
            open_data_frame(&mut rt, &frames[0]),
            Err(RemoteError::Noise(_))
        ));
    }

    #[test]
    fn outer_metadata_rewrite_is_rejected() {
        let (mut it, mut rt, src, dst) = transport_pair();
        let mut frames = seal_engine_message(&mut it, src, dst, 42, b"payload", 64).unwrap();
        // Relay rewrites the clear message_id but cannot touch the sealed copy.
        frames[0].message_id = 99;
        assert!(matches!(
            open_data_frame(&mut rt, &frames[0]),
            Err(RemoteError::Noise(
                crate::noise::NoiseError::MetadataMismatch
            ))
        ));
    }

    #[test]
    fn chunk_count_rewrite_is_rejected() {
        let (mut it, mut rt, src, dst) = transport_pair();
        // Force two chunks.
        let mut frames = seal_engine_message(&mut it, src, dst, 5, b"twelve bytes", 6).unwrap();
        assert_eq!(frames.len(), 2);
        // Relay inflates the clear chunk_count; the bound copy still says 2.
        frames[0].chunk_count = 3;
        assert!(matches!(
            open_data_frame(&mut rt, &frames[0]),
            Err(RemoteError::ChunkCountMismatch { outer: 3, inner: 2 })
        ));
    }
}
