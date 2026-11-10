//! Integration tests for `sqlly-engine-protocol` framing and binary cursors.
//! Mirrors the structure of `sqlly-wire/tests/wire.rs`.

use std::io::Cursor;

use sqlly_remote::engine_protocol::{
    BinaryReader, BinaryWriter, FRAME_TAG_EVENT, FRAME_TAG_READY, MAX_FRAME_BYTES, ProtocolError,
    RawFrame, encode_envelope, encode_id_prefix, frame_id, read_length_prefixed,
    read_length_prefixed_blocking,
};

// ---------------------------------------------------------------------------
// Byte-exact vectors
// ---------------------------------------------------------------------------

#[test]
fn encode_envelope_is_byte_exact() {
    let body = b"hello";
    let wire = encode_envelope(0x42, body).unwrap();
    assert_eq!(wire, [6, 0, 0, 0, 0x42, b'h', b'e', b'l', b'l', b'o']);
}

#[test]
fn encode_id_prefix_is_byte_exact() {
    let prefix = encode_id_prefix("abc").unwrap();
    assert_eq!(prefix, [3, 0, 0, 0, b'a', b'b', b'c']);
}

#[test]
fn encode_envelope_empty_body_is_byte_exact() {
    let wire = encode_envelope(FRAME_TAG_READY, &[]).unwrap();
    assert_eq!(wire, [1, 0, 0, 0, FRAME_TAG_READY]);
}

// ---------------------------------------------------------------------------
// Round-trips
// ---------------------------------------------------------------------------

#[test]
fn raw_frame_payload_round_trip() {
    let body = encode_id_prefix("req-9").unwrap();
    let frame = RawFrame::new(FRAME_TAG_EVENT, body);
    let payload = frame.to_payload();
    let decoded = RawFrame::from_payload(&payload).unwrap();
    assert_eq!(decoded, frame);
}

#[test]
fn frame_id_parses_leading_id_prefix() {
    let body = encode_id_prefix("abc").unwrap();
    assert_eq!(frame_id(FRAME_TAG_EVENT, &body).as_deref(), Some("abc"));
}

#[test]
fn ready_frame_has_no_id() {
    let body = [1, 0, 0, 0]; // protocol version bytes, not an id
    assert_eq!(frame_id(FRAME_TAG_READY, &body), None);
    let frame = RawFrame::new(FRAME_TAG_READY, body.to_vec());
    assert_eq!(frame.id(), None);
}

#[test]
fn empty_payload_is_rejected() {
    assert!(matches!(
        RawFrame::from_payload(&[]),
        Err(ProtocolError::EmptyFrame)
    ));
}

#[test]
fn encode_envelope_rejects_body_past_max_frame_bytes() {
    let huge = vec![0u8; MAX_FRAME_BYTES as usize];
    let err = encode_envelope(FRAME_TAG_EVENT, &huge).unwrap_err();
    assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
}

// ---------------------------------------------------------------------------
// Truncation + oversize (async)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn async_read_clean_eof_before_frame_returns_none() {
    let mut cursor = Cursor::new([]);
    assert!(read_length_prefixed(&mut cursor).await.unwrap().is_none());
}

#[tokio::test]
async fn async_read_partial_length_prefix_eof_returns_none() {
    let mut cursor = Cursor::new([1, 2]);
    assert!(read_length_prefixed(&mut cursor).await.unwrap().is_none());
}

#[tokio::test]
async fn async_read_partial_body_errors() {
    let payload = RawFrame::new(FRAME_TAG_EVENT, encode_id_prefix("x").unwrap()).to_payload();
    let wire = encode_envelope(FRAME_TAG_EVENT, &payload[1..]).unwrap();
    let truncated = wire[..wire.len() - 2].to_vec();
    let mut cursor = Cursor::new(truncated);
    let err = read_length_prefixed(&mut cursor).await.unwrap_err();
    assert!(matches!(err, ProtocolError::Io(_)));
}

#[tokio::test]
async fn async_read_oversize_length_rejects_without_allocating_claimed_body() {
    let oversize = (MAX_FRAME_BYTES + 1).to_le_bytes();
    let mut cursor = Cursor::new(oversize);
    let err = read_length_prefixed(&mut cursor).await.unwrap_err();
    assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
}

#[tokio::test]
async fn async_split_read_reassembles_frame() {
    let payload = RawFrame::new(FRAME_TAG_EVENT, encode_id_prefix("split").unwrap()).to_payload();
    let wire = encode_envelope(FRAME_TAG_EVENT, &payload[1..]).unwrap();
    let split = wire.len() / 2;
    let mut mock = tokio_test::io::Builder::new()
        .read(&wire[..split])
        .read(&wire[split..])
        .build();
    let got = read_length_prefixed(&mut mock).await.unwrap().unwrap();
    let frame = RawFrame::from_payload(&got).unwrap();
    assert_eq!(frame.tag, FRAME_TAG_EVENT);
    assert_eq!(frame.id().as_deref(), Some("split"));
}

// ---------------------------------------------------------------------------
// Blocking truncation mirror
// ---------------------------------------------------------------------------

#[test]
fn blocking_read_clean_eof_returns_none() {
    let mut cursor = Cursor::new([]);
    assert!(
        read_length_prefixed_blocking(&mut cursor)
            .unwrap()
            .is_none()
    );
}

#[test]
fn blocking_read_oversize_length_rejects() {
    let oversize = (MAX_FRAME_BYTES + 1).to_le_bytes();
    let mut cursor = Cursor::new(oversize);
    let err = read_length_prefixed_blocking(&mut cursor).unwrap_err();
    assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
}

// ---------------------------------------------------------------------------
// BinaryReader / BinaryWriter
// ---------------------------------------------------------------------------

#[test]
fn binary_reader_walks_hand_built_buffer() {
    let body = BinaryWriter::new()
        .u8(7)
        .u16_le(0x0102)
        .u32_le(0x03040506)
        .string("hello")
        .bool(false)
        .done();

    let mut reader = BinaryReader::new(&body);
    assert_eq!(reader.read_u8("a").unwrap(), 7);
    assert_eq!(reader.read_u16_le("b").unwrap(), 0x0102);
    assert_eq!(reader.read_u32_le("c").unwrap(), 0x03040506);
    assert_eq!(reader.read_string("d").unwrap(), "hello");
    assert!(!reader.read_bool("e").unwrap());
    reader.ensure_at_end().unwrap();
}

#[test]
fn binary_reader_truncation_error_names_field() {
    let mut reader = BinaryReader::new(&[1, 2]);
    let err = reader.read_u32_le("width").unwrap_err().to_string();
    assert!(
        err.contains("truncated") && err.contains("'width'"),
        "{err}"
    );
}

#[test]
fn binary_writer_round_trip_extended() {
    let body = BinaryWriter::new()
        .i64_le(-42)
        .f64_le(1.5)
        .opt_string(Some("x"))
        .opt_string(None)
        .opt_i64(Some(99))
        .opt_u32(None)
        .done();

    let mut reader = BinaryReader::new(&body);
    assert_eq!(reader.read_i64_le("i").unwrap(), -42);
    assert_eq!(reader.read_f64_le("f").unwrap(), 1.5);
    assert_eq!(
        reader.read_optional_string("s1").unwrap().as_deref(),
        Some("x")
    );
    assert_eq!(reader.read_optional_string("s2").unwrap(), None);
    assert_eq!(reader.read_optional_i64("i").unwrap(), Some(99));
    assert_eq!(reader.read_optional_u32("u").unwrap(), None);
    reader.ensure_at_end().unwrap();
}
