//! Integration tests for the `sqlly-wire` public API: relay frame codec,
//! chunking / reassembly, and capacity math.

use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use uuid::Uuid;

use sqlly_wire::{
    AssembleError, AssemblerConfig, ChunkAssembler, ChunkError, FLAG_LAST_CHUNK, FrameKind,
    HEADER_LEN, MAX_BODY_LEN, PROTOCOL_VERSION, RelayErrorCode, RelayFrame, WireError,
    capacity_pct, split_into_chunks,
};

fn ids() -> (Uuid, Uuid) {
    (
        Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
        Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Capacity math
// ---------------------------------------------------------------------------

#[test]
fn capacity_required_vectors() {
    assert_eq!(capacity_pct(0, 2000), 0);
    assert_eq!(capacity_pct(1, 2000), 0);
    assert_eq!(capacity_pct(10, 2000), 1);
    assert_eq!(capacity_pct(1999, 2000), 99);
    assert_eq!(capacity_pct(2000, 2000), 100);
    assert_eq!(capacity_pct(2001, 2000), 100);
    assert_eq!(capacity_pct(5, 0), 100);
}

// ---------------------------------------------------------------------------
// Codec round-trips and field placement
// ---------------------------------------------------------------------------

#[test]
fn round_trip_data_frame() {
    let (src, dst) = ids();
    let frame = RelayFrame::data(src, dst, 42, 0, 1, Bytes::from_static(b"hello relay"));
    let encoded = frame.encode().unwrap();
    assert_eq!(encoded.len(), HEADER_LEN + b"hello relay".len());

    let decoded = RelayFrame::decode_exact(encoded).unwrap();
    assert_eq!(decoded, frame);
    assert_eq!(decoded.kind, FrameKind::Data);
    assert_eq!(decoded.source, src);
    assert_eq!(decoded.destination, dst);
    assert_eq!(decoded.message_id, 42);
    assert_eq!(&decoded.body[..], b"hello relay");
    // Single-chunk data frame marks itself as the last chunk.
    assert_eq!(decoded.flags & FLAG_LAST_CHUNK, FLAG_LAST_CHUNK);
}

#[test]
fn round_trip_empty_body() {
    let (src, dst) = ids();
    let frame = RelayFrame::ping(src, dst, 7);
    let encoded = frame.encode().unwrap();
    assert_eq!(encoded.len(), HEADER_LEN);
    let decoded = RelayFrame::decode_exact(encoded).unwrap();
    assert_eq!(decoded, frame);
    assert!(decoded.body.is_empty());
}

#[test]
fn round_trip_all_kinds() {
    let (src, dst) = ids();
    for kind in [
        FrameKind::Auth,
        FrameKind::NoiseHandshake,
        FrameKind::Data,
        FrameKind::Ping,
        FrameKind::Pong,
        FrameKind::RelayError,
        FrameKind::Close,
        FrameKind::RouteRevoked,
    ] {
        let frame = RelayFrame::new(kind, src, dst, 1, Bytes::from_static(b"x"));
        let decoded = RelayFrame::decode_exact(frame.encode().unwrap()).unwrap();
        assert_eq!(decoded.kind, kind);
    }
}

#[test]
fn uuid_field_placement_is_byte_exact() {
    let (src, dst) = ids();
    let frame = RelayFrame::data(src, dst, 0, 0, 1, Bytes::new());
    let bytes = frame.encode().unwrap();
    // source occupies offset 8..24, destination 24..40 (raw canonical bytes).
    assert_eq!(&bytes[8..24], src.as_bytes());
    assert_eq!(&bytes[24..40], dst.as_bytes());
    // protocol_version at offset 4..6, little-endian.
    assert_eq!(&bytes[4..6], &PROTOCOL_VERSION.to_le_bytes());
    // frame_kind byte at offset 6.
    assert_eq!(bytes[6], FrameKind::Data as u8);
}

#[test]
fn header_multibyte_fields_are_little_endian() {
    let (src, dst) = ids();
    let frame = RelayFrame::data(
        src,
        dst,
        0x0102_0304_0506_0708,
        0x0a0b_0c0d,
        0x1112_1314,
        Bytes::new(),
    );
    let b = frame.encode().unwrap();
    assert_eq!(&b[40..48], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(&b[48..52], &0x0a0b_0c0du32.to_le_bytes());
    assert_eq!(&b[52..56], &0x1112_1314u32.to_le_bytes());
}

#[test]
fn relay_error_frame_round_trip() {
    let (relay, client) = ids();
    for code in [
        RelayErrorCode::DestOffline,
        RelayErrorCode::RoutingDenied,
        RelayErrorCode::AuthFailed,
        RelayErrorCode::ProtocolError,
        RelayErrorCode::RateLimited,
        RelayErrorCode::RelayFull,
    ] {
        let frame = RelayFrame::relay_error(relay, client, 9, code);
        let decoded = RelayFrame::decode_exact(frame.encode().unwrap()).unwrap();
        assert_eq!(decoded.kind, FrameKind::RelayError);
        assert_eq!(decoded.relay_error_code(), Some(code));
    }
}

#[test]
fn streaming_decode_consumes_one_frame_and_leaves_the_rest() {
    let (src, dst) = ids();
    let f1 = RelayFrame::data(src, dst, 1, 0, 1, Bytes::from_static(b"aaa"));
    let f2 = RelayFrame::ping(src, dst, 2);

    let mut buf = BytesMut::new();
    f1.encode_to(&mut buf).unwrap();
    f2.encode_to(&mut buf).unwrap();
    let mut stream = buf.freeze();

    let d1 = RelayFrame::decode(&mut stream).unwrap().unwrap();
    assert_eq!(d1, f1);
    let d2 = RelayFrame::decode(&mut stream).unwrap().unwrap();
    assert_eq!(d2, f2);
    assert!(stream.is_empty());
    assert!(RelayFrame::decode(&mut stream).unwrap().is_none());
}

#[test]
fn streaming_decode_returns_none_until_complete() {
    let (src, dst) = ids();
    let frame = RelayFrame::data(src, dst, 1, 0, 1, Bytes::from_static(b"partial payload"));
    let full = frame.encode().unwrap();

    // Only the length prefix present.
    let mut partial = full.slice(0..4);
    assert!(RelayFrame::decode(&mut partial).unwrap().is_none());

    // Header present but body truncated.
    let mut partial = full.slice(0..HEADER_LEN + 2);
    assert!(RelayFrame::decode(&mut partial).unwrap().is_none());
    // Untouched on incomplete.
    assert_eq!(partial.len(), HEADER_LEN + 2);

    // Fewer than 4 bytes.
    let mut partial = full.slice(0..3);
    assert!(RelayFrame::decode(&mut partial).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Codec rejection paths
// ---------------------------------------------------------------------------

#[test]
fn rejects_unknown_protocol_version() {
    let (src, dst) = ids();
    let mut bytes = BytesMut::from(&RelayFrame::ping(src, dst, 1).encode().unwrap()[..]);
    // Overwrite the version field (offset 4..6) with an unsupported version.
    bytes[4] = 0xFF;
    bytes[5] = 0xFF;
    let err = RelayFrame::decode_exact(bytes.freeze()).unwrap_err();
    assert_eq!(err, WireError::UnknownVersion(0xFFFF));
}

#[test]
fn rejects_unknown_frame_kind() {
    let (src, dst) = ids();
    let mut bytes = BytesMut::from(&RelayFrame::ping(src, dst, 1).encode().unwrap()[..]);
    bytes[6] = 200; // frame_kind byte
    let err = RelayFrame::decode_exact(bytes.freeze()).unwrap_err();
    assert_eq!(err, WireError::UnknownFrameKind(200));
}

#[test]
fn rejects_reserved_flags_on_decode() {
    let (src, dst) = ids();
    let mut bytes = BytesMut::from(&RelayFrame::ping(src, dst, 1).encode().unwrap()[..]);
    bytes[7] = 0b1000_0000; // reserved flag bit
    let err = RelayFrame::decode_exact(bytes.freeze()).unwrap_err();
    assert_eq!(err, WireError::ReservedFlags(0b1000_0000));
}

#[test]
fn rejects_reserved_flags_on_encode() {
    let (src, dst) = ids();
    let mut frame = RelayFrame::ping(src, dst, 1);
    frame.flags = 0b0100_0000;
    let err = frame.encode().unwrap_err();
    assert_eq!(err, WireError::ReservedFlags(0b0100_0000));
}

#[test]
fn rejects_body_length_mismatch() {
    let (src, dst) = ids();
    let frame = RelayFrame::data(src, dst, 1, 0, 1, Bytes::from_static(b"1234"));
    let mut bytes = BytesMut::from(&frame.encode().unwrap()[..]);
    // Corrupt the body_len field (offset 56..60) so it disagrees with frame_len.
    bytes[56] = 99;
    let err = RelayFrame::decode_exact(bytes.freeze()).unwrap_err();
    assert!(
        matches!(err, WireError::LengthMismatch { .. }),
        "got {err:?}"
    );
}

#[test]
fn rejects_oversized_frame_on_decode() {
    let (src, dst) = ids();
    // Craft a header whose frame_len prefix claims a body over the max, without
    // actually supplying the bytes: decode must reject before waiting for them.
    let frame = RelayFrame::ping(src, dst, 1);
    let mut bytes = BytesMut::from(&frame.encode().unwrap()[..]);
    let oversized = (sqlly_wire::MIN_FRAME_LEN + MAX_BODY_LEN + 1) as u32;
    bytes[0..4].copy_from_slice(&oversized.to_le_bytes());
    let err = RelayFrame::decode(&mut bytes.freeze()).unwrap_err();
    assert!(matches!(err, WireError::Oversized { .. }), "got {err:?}");
}

#[test]
fn rejects_oversized_body_on_encode() {
    let (src, dst) = ids();
    let mut frame = RelayFrame::data(src, dst, 1, 0, 1, Bytes::new());
    frame.body = Bytes::from(vec![0u8; MAX_BODY_LEN + 1]);
    let err = frame.encode().unwrap_err();
    assert!(matches!(err, WireError::Oversized { .. }), "got {err:?}");
}

#[test]
fn rejects_frame_len_too_small() {
    // A frame_len smaller than the header cannot be valid.
    let mut bytes = BytesMut::new();
    bytes.extend_from_slice(&10u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 10]);
    let err = RelayFrame::decode(&mut bytes.freeze()).unwrap_err();
    assert!(matches!(err, WireError::Truncated { .. }), "got {err:?}");
}

#[test]
fn rejects_nil_source_id() {
    let (_, dst) = ids();
    let frame = RelayFrame::data(Uuid::nil(), dst, 1, 0, 1, Bytes::new());
    assert_eq!(frame.encode().unwrap_err(), WireError::NilSourceId);
}

#[test]
fn rejects_nil_destination_id() {
    let (src, _) = ids();
    let frame = RelayFrame::data(src, Uuid::nil(), 1, 0, 1, Bytes::new());
    assert_eq!(frame.encode().unwrap_err(), WireError::NilDestinationId);
}

#[test]
fn rejects_nil_ids_on_decode() {
    // Encode a valid frame, then zero out the source id bytes (offset 8..24).
    let (src, dst) = ids();
    let mut bytes = BytesMut::from(
        &RelayFrame::data(src, dst, 1, 0, 1, Bytes::new())
            .encode()
            .unwrap()[..],
    );
    for b in &mut bytes[8..24] {
        *b = 0;
    }
    let err = RelayFrame::decode_exact(bytes.freeze()).unwrap_err();
    assert_eq!(err, WireError::NilSourceId);
}

// ---------------------------------------------------------------------------
// Chunking: split_into_chunks
// ---------------------------------------------------------------------------

#[test]
fn split_empty_payload_yields_one_empty_chunk() {
    let chunks = split_into_chunks(&[], 16).unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_empty());
}

#[test]
fn split_exact_max_yields_single_chunk() {
    let data = vec![7u8; 16];
    let chunks = split_into_chunks(&data, 16).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(&chunks[0][..], &data[..]);
}

#[test]
fn split_one_byte_over_max_yields_two_chunks() {
    let data = vec![7u8; 17];
    let chunks = split_into_chunks(&data, 16).unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].len(), 16);
    assert_eq!(chunks[1].len(), 1);
}

#[test]
fn split_multiple_chunks_reconstructs_in_order() {
    let data: Vec<u8> = (0..100).collect();
    let chunks = split_into_chunks(&data, 16).unwrap();
    assert_eq!(chunks.len(), 7); // ceil(100/16)
    let mut rebuilt = Vec::new();
    for c in &chunks {
        rebuilt.extend_from_slice(c);
    }
    assert_eq!(rebuilt, data);
}

#[test]
fn split_rejects_zero_max_body() {
    assert_eq!(
        split_into_chunks(b"abc", 0).unwrap_err(),
        ChunkError::ZeroMaxBody
    );
}

// ---------------------------------------------------------------------------
// Chunking: ChunkAssembler
// ---------------------------------------------------------------------------

fn test_config() -> AssemblerConfig {
    AssemblerConfig {
        max_message_size: 1024,
        max_in_flight: 4,
        max_chunk_count: 1024,
        timeout: Duration::from_secs(10),
    }
}

#[test]
fn assembler_reassembles_in_order() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    let data: Vec<u8> = (0..50).collect();
    let chunks = split_into_chunks(&data, 16).unwrap();
    let count = chunks.len() as u32;

    let mut result = None;
    for (i, c) in chunks.into_iter().enumerate() {
        result = a.push(1, i as u32, count, c, now).unwrap();
    }
    assert_eq!(result.unwrap()[..], data[..]);
    assert_eq!(a.in_flight(), 0);
}

#[test]
fn assembler_reassembles_out_of_order() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    let data: Vec<u8> = (0..50).collect();
    let chunks = split_into_chunks(&data, 16).unwrap();
    let count = chunks.len() as u32;
    assert_eq!(count, 4);

    // Deliver in reverse order; only the final delivery completes.
    assert!(
        a.push(1, 3, count, chunks[3].clone(), now)
            .unwrap()
            .is_none()
    );
    assert!(
        a.push(1, 1, count, chunks[1].clone(), now)
            .unwrap()
            .is_none()
    );
    assert!(
        a.push(1, 2, count, chunks[2].clone(), now)
            .unwrap()
            .is_none()
    );
    let done = a
        .push(1, 0, count, chunks[0].clone(), now)
        .unwrap()
        .unwrap();
    assert_eq!(done[..], data[..]);
}

#[test]
fn assembler_single_chunk_completes_immediately() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    let done = a.push(7, 0, 1, Bytes::from_static(b"solo"), now).unwrap();
    assert_eq!(&done.unwrap()[..], b"solo");
    assert_eq!(a.in_flight(), 0);
}

#[test]
fn assembler_single_empty_chunk_completes() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    let done = a.push(7, 0, 1, Bytes::new(), now).unwrap();
    assert!(done.unwrap().is_empty());
}

#[test]
fn assembler_detects_duplicate_chunk() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    assert!(
        a.push(1, 0, 3, Bytes::from_static(b"aa"), now)
            .unwrap()
            .is_none()
    );
    let err = a.push(1, 0, 3, Bytes::from_static(b"aa"), now).unwrap_err();
    assert_eq!(
        err,
        AssembleError::DuplicateChunk {
            message_id: 1,
            index: 0
        }
    );
}

#[test]
fn assembler_rejects_index_out_of_range() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    let err = a.push(1, 5, 3, Bytes::from_static(b"x"), now).unwrap_err();
    assert_eq!(
        err,
        AssembleError::IndexOutOfRange {
            message_id: 1,
            index: 5,
            count: 3
        }
    );
}

#[test]
fn assembler_rejects_zero_chunk_count() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    let err = a.push(1, 0, 0, Bytes::new(), now).unwrap_err();
    assert_eq!(err, AssembleError::ZeroChunkCount { message_id: 1 });
}

#[test]
fn assembler_rejects_inconsistent_chunk_count() {
    let mut a = ChunkAssembler::new(test_config());
    let now = Instant::now();
    assert!(
        a.push(1, 0, 3, Bytes::from_static(b"aa"), now)
            .unwrap()
            .is_none()
    );
    let err = a.push(1, 1, 4, Bytes::from_static(b"bb"), now).unwrap_err();
    assert_eq!(
        err,
        AssembleError::InconsistentChunkCount {
            message_id: 1,
            existing: 3,
            got: 4
        }
    );
}

#[test]
fn assembler_missing_chunk_times_out() {
    let mut a = ChunkAssembler::new(test_config());
    let t0 = Instant::now();
    // Two of three chunks arrive; the third never does.
    assert!(
        a.push(1, 0, 3, Bytes::from_static(b"aa"), t0)
            .unwrap()
            .is_none()
    );
    assert!(
        a.push(1, 1, 3, Bytes::from_static(b"bb"), t0)
            .unwrap()
            .is_none()
    );
    assert_eq!(a.in_flight(), 1);

    // Before the timeout, nothing expires.
    let before = a.expire(t0 + Duration::from_secs(5));
    assert!(before.is_empty());
    assert_eq!(a.in_flight(), 1);

    // After the timeout, the stalled message is dropped and reported.
    let timed_out = a.expire(t0 + Duration::from_secs(11));
    assert_eq!(timed_out.len(), 1);
    assert_eq!(timed_out[0].message_id, 1);
    assert_eq!(timed_out[0].received, 2);
    assert_eq!(timed_out[0].expected, 3);
    assert_eq!(a.in_flight(), 0);
}

#[test]
fn assembler_rejects_message_over_max_reconstructed_size() {
    let cfg = AssemblerConfig {
        max_message_size: 20,
        ..test_config()
    };
    let mut a = ChunkAssembler::new(cfg);
    let now = Instant::now();
    // First chunk (12 bytes) fits.
    assert!(
        a.push(1, 0, 2, Bytes::from(vec![0u8; 12]), now)
            .unwrap()
            .is_none()
    );
    // Second chunk (12 bytes) would push total to 24 > 20 -> rejected.
    let err = a
        .push(1, 1, 2, Bytes::from(vec![0u8; 12]), now)
        .unwrap_err();
    assert_eq!(
        err,
        AssembleError::MessageTooLarge {
            message_id: 1,
            max: 20
        }
    );
    // The whole message is dropped so it cannot linger.
    assert_eq!(a.in_flight(), 0);
}

#[test]
fn assembler_single_chunk_over_max_size_rejected() {
    let cfg = AssemblerConfig {
        max_message_size: 8,
        ..test_config()
    };
    let mut a = ChunkAssembler::new(cfg);
    let now = Instant::now();
    let err = a.push(1, 0, 1, Bytes::from(vec![0u8; 9]), now).unwrap_err();
    assert_eq!(
        err,
        AssembleError::MessageTooLarge {
            message_id: 1,
            max: 8
        }
    );
}

#[test]
fn assembler_enforces_max_in_flight() {
    let cfg = AssemblerConfig {
        max_in_flight: 2,
        ..test_config()
    };
    let mut a = ChunkAssembler::new(cfg);
    let now = Instant::now();
    // Start two multi-chunk messages (each holds a slot).
    assert!(
        a.push(1, 0, 2, Bytes::from_static(b"a"), now)
            .unwrap()
            .is_none()
    );
    assert!(
        a.push(2, 0, 2, Bytes::from_static(b"b"), now)
            .unwrap()
            .is_none()
    );
    assert_eq!(a.in_flight(), 2);
    // A third distinct message exceeds the in-flight bound.
    let err = a.push(3, 0, 2, Bytes::from_static(b"c"), now).unwrap_err();
    assert_eq!(err, AssembleError::TooManyInFlight { max: 2 });
    // Adding a chunk to an existing message is still allowed.
    assert!(
        a.push(1, 1, 2, Bytes::from_static(b"a2"), now)
            .unwrap()
            .is_some()
    );
}

#[test]
fn assembler_rejects_chunk_count_over_max() {
    let cfg = AssemblerConfig {
        max_chunk_count: 4,
        ..test_config()
    };
    let mut a = ChunkAssembler::new(cfg);
    let now = Instant::now();
    let err = a.push(1, 0, 5, Bytes::from_static(b"x"), now).unwrap_err();
    assert_eq!(
        err,
        AssembleError::ChunkCountTooLarge {
            message_id: 1,
            count: 5,
            max: 4
        }
    );
}

#[test]
fn round_trip_through_chunking_and_reassembly() {
    // Full loop: split a payload, wrap each chunk in a Data frame, encode,
    // decode, and reassemble — the relay's opaque forwarding path.
    let (src, dst) = ids();
    let payload: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
    let chunks = split_into_chunks(&payload, 128).unwrap();
    let count = chunks.len() as u32;

    let mut a = ChunkAssembler::new(AssemblerConfig {
        max_message_size: 4096,
        ..test_config()
    });
    let now = Instant::now();
    let mut done = None;
    for (i, chunk) in chunks.into_iter().enumerate() {
        let frame = RelayFrame::data(src, dst, 99, i as u32, count, chunk);
        let wire = frame.encode().unwrap();
        let decoded = RelayFrame::decode_exact(wire).unwrap();
        assert_eq!(decoded.source, src);
        assert_eq!(decoded.destination, dst);
        done = a
            .push(
                decoded.message_id,
                decoded.chunk_index,
                decoded.chunk_count,
                decoded.body,
                now,
            )
            .unwrap();
    }
    assert_eq!(done.unwrap()[..], payload[..]);
}
