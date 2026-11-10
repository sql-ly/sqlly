//! `sqlly-engine-protocol`: the shared framing/codec for the SQLLY engine wire
//! protocol, extracted from `apps/cli/src/engine.rs` so the CLI, GPUI remote
//! sessions, the bridge, and tests all speak one codec.
//!
//! Scope: frame tags, protocol/layout version constants, length-prefixed frame
//! read/write, the low-level binary reader/writer cursors, the frame envelope,
//! terminal-frame recognition, size limits, and an `EngineClient<T>` over an
//! abstract framed transport. Engine request *dispatch*, database access, and
//! the concrete `EngineFrame`/`EngineRequest` domain enums stay in `sqlly-cli`.

#![forbid(unsafe_code)]

pub mod capabilities;
mod client;
mod constants;
mod error;
mod framing;
pub mod layouts;
mod reader;
mod writer;

pub use capabilities::{
    CAPABILITY_QUERY_CELL_PREVIEWS, CAPABILITY_ROW_EDIT_APPLY_BATCH, CAPABILITY_ROW_EDIT_FK_LABELS,
    CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS, EngineCapabilities, ReadyInfo, decode_ready_body,
    encode_ready_body,
};
#[cfg(not(target_arch = "wasm32"))]
pub use client::AsyncFramedTransport;
pub use client::{EngineClient, EngineClientError, FramedTransport};
pub use constants::{
    BINARY_REQUEST_LAYOUT_VERSION, EXECUTE_QUERY_OPTION_CELL_PREVIEWS,
    FRAME_TAG_AI_CACHE_BUILD_REPORT, FRAME_TAG_CATALOG_MAINTENANCE_REPORT,
    FRAME_TAG_CATALOG_SYNC_REPORT, FRAME_TAG_EMBEDDINGS_BUILD_REPORT, FRAME_TAG_EVENT,
    FRAME_TAG_INVALIDATE_AI_CACHE_REPORT, FRAME_TAG_JOIN_PATH_REPORT, FRAME_TAG_PONG,
    FRAME_TAG_PROVIDER_STATUS_REPORT, FRAME_TAG_QUERY_SUMMARY_REPORT, FRAME_TAG_READY,
    FRAME_TAG_REQUEST_CANCELLED, FRAME_TAG_REQUEST_COMPLETED, FRAME_TAG_REQUEST_FAILED,
    FRAME_TAG_REQUEST_STARTED, FRAME_TAG_ROW_EDIT_REPORT, FRAME_TAG_ROWS_BATCH,
    FRAME_TAG_SCHEMA_ANSWER, FRAME_TAG_SCHEMA_SEARCH_RESULTS, FRAME_TAG_SHUTDOWN_ACK,
    FRAME_TAG_SQL_GENERATION_REPORT, FRAME_TAG_SQL_REVIEW_REPORT, FRAME_TAG_SQL_VALIDATION_REPORT,
    FRAME_TAG_TRUNCATED_CELLS, FRAME_TAG_VALIDATION, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    REQUEST_TAG_ASK_SCHEMA, REQUEST_TAG_BUILD_AI_CACHE, REQUEST_TAG_BUILD_EMBEDDINGS,
    REQUEST_TAG_CANCEL, REQUEST_TAG_CATALOG_MAINTENANCE, REQUEST_TAG_EXECUTE_QUERY,
    REQUEST_TAG_FIND_JOIN_PATH, REQUEST_TAG_GENERATE_SQL, REQUEST_TAG_INVALIDATE_AI_CACHE,
    REQUEST_TAG_PING, REQUEST_TAG_PROVIDER_STATUS, REQUEST_TAG_REVIEW_SQL, REQUEST_TAG_ROW_EDIT,
    REQUEST_TAG_SEARCH_SCHEMA, REQUEST_TAG_SHUTDOWN, REQUEST_TAG_SUMMARIZE_QUERY,
    REQUEST_TAG_SYNC_CATALOG, REQUEST_TAG_VALIDATE, REQUEST_TAG_VALIDATE_SQL,
    is_terminal_frame_tag,
};
pub use error::{ProtocolError, ProtocolResult};
pub use framing::{
    RawFrame, encode_envelope, encode_id_prefix, frame_id, read_length_prefixed_blocking,
    write_length_prefixed_blocking,
};
#[cfg(not(target_arch = "wasm32"))]
pub use framing::{read_length_prefixed, write_length_prefixed};
pub use layouts::{
    CatalogColumnWire, CatalogForeignKeyWire, CatalogObjectWire, CatalogParameterWire,
    ExecuteQueryWire, RowsBatchWire, SyncCatalogWire, TenantScopeWire, TruncatedCellWire,
    TruncatedCellsWire, WireCell, append_tenant_scope_trailing,
    decode_tenant_scope_trailing_if_present,
};
pub use reader::BinaryReader;
pub use writer::BinaryWriter;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::time::{Duration, Instant};

    // -------------------------------------------------------------------
    // BinaryReader / BinaryWriter round trips + error messages
    // -------------------------------------------------------------------

    #[test]
    fn reader_writer_round_trip() {
        let body = BinaryWriter::new()
            .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
            .string("req-1")
            .opt_string(Some("dbo"))
            .opt_string(None)
            .u32_le(2)
            .i64_le(-7)
            .f64_le(2.5)
            .bool(true)
            .opt_i64(Some(9))
            .opt_u32(None)
            .done();

        let mut r = BinaryReader::new(&body);
        assert_eq!(r.read_u16_le("v").unwrap(), BINARY_REQUEST_LAYOUT_VERSION);
        assert_eq!(r.read_string("id").unwrap(), "req-1");
        assert_eq!(r.read_optional_string("a").unwrap().as_deref(), Some("dbo"));
        assert_eq!(r.read_optional_string("b").unwrap(), None);
        assert_eq!(r.read_u32_le("c").unwrap(), 2);
        assert_eq!(r.read_i64_le("d").unwrap(), -7);
        assert_eq!(r.read_f64_le("e").unwrap(), 2.5);
        assert!(r.read_bool("f").unwrap());
        assert_eq!(r.read_optional_i64("g").unwrap(), Some(9));
        assert_eq!(r.read_optional_u32("h").unwrap(), None);
        r.ensure_at_end().unwrap();
    }

    #[test]
    fn reader_error_messages_match_engine_contract() {
        // These substrings are asserted on by sqlly-cli's tests.
        let mut r = BinaryReader::new(&[]);
        let err = r.read_u8("f").unwrap_err().to_string();
        assert!(err.contains("truncated") && err.contains("'f'"), "{err}");

        let mut r = BinaryReader::new(&[0x02]);
        assert!(
            r.read_bool("flag")
                .unwrap_err()
                .to_string()
                .contains("invalid bool")
        );

        let mut r = BinaryReader::new(&[0x02]);
        assert!(
            r.read_optional_string("opt")
                .unwrap_err()
                .to_string()
                .contains("invalid presence")
        );

        let data: Vec<u8> = [100u32.to_le_bytes().as_slice(), b"abc"].concat();
        let mut r = BinaryReader::new(&data);
        assert!(
            r.read_string("s")
                .unwrap_err()
                .to_string()
                .contains("declared 100 bytes")
        );

        let data: Vec<u8> = [999u32.to_le_bytes().as_slice(), &[0, 0]].concat();
        let mut r = BinaryReader::new(&data);
        assert!(
            r.read_count("n")
                .unwrap_err()
                .to_string()
                .contains("999 elements")
        );

        let mut r = BinaryReader::new(&[0x01, 0x02]);
        r.read_u8("a").unwrap();
        assert!(
            r.ensure_at_end()
                .unwrap_err()
                .to_string()
                .contains("1 trailing")
        );
    }

    #[test]
    fn read_count_with_min_rejects_footprint_and_overflow() {
        let buf = BinaryWriter::new().u32_le(2).u8(0).u8(0).done();
        let mut r = BinaryReader::new(&buf);
        let err = r
            .read_count_with_min("test_count", 17)
            .unwrap_err()
            .to_string();
        assert!(err.contains("test_count") && err.contains("34"), "{err}");

        let buf = BinaryWriter::new().u32_le(2).done();
        let mut r = BinaryReader::new(&buf);
        assert!(
            r.read_count_with_min("overflow_count", usize::MAX / 2 + 1)
                .unwrap_err()
                .to_string()
                .contains("overflows")
        );
    }

    // -------------------------------------------------------------------
    // Envelope + framing
    // -------------------------------------------------------------------

    #[test]
    fn envelope_round_trip_and_id_extraction() {
        let body = {
            let mut b = encode_id_prefix("q-42").unwrap();
            b.extend_from_slice(b"payload");
            b
        };
        let wire = encode_envelope(FRAME_TAG_EVENT, &body).unwrap();

        // [u32 total_len][tag][body]
        let total = u32::from_le_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
        assert_eq!(total, body.len() + 1);
        assert_eq!(wire[4], FRAME_TAG_EVENT);

        let frame = RawFrame::from_payload(&wire[4..]).unwrap();
        assert_eq!(frame.tag, FRAME_TAG_EVENT);
        assert_eq!(frame.id().as_deref(), Some("q-42"));
        assert!(!frame.is_terminal());
        assert_eq!(frame.encode().unwrap(), wire);
    }

    #[test]
    fn ready_frame_has_no_id() {
        let body = PROTOCOL_VERSION.to_le_bytes().to_vec();
        assert_eq!(frame_id(FRAME_TAG_READY, &body), None);
    }

    #[test]
    fn terminal_recognition() {
        assert!(is_terminal_frame_tag(FRAME_TAG_REQUEST_COMPLETED));
        assert!(is_terminal_frame_tag(FRAME_TAG_REQUEST_FAILED));
        assert!(is_terminal_frame_tag(FRAME_TAG_REQUEST_CANCELLED));
        assert!(!is_terminal_frame_tag(FRAME_TAG_ROWS_BATCH));
        assert!(!is_terminal_frame_tag(FRAME_TAG_PONG));
    }

    #[test]
    fn empty_payload_is_rejected() {
        assert!(matches!(
            RawFrame::from_payload(&[]),
            Err(ProtocolError::EmptyFrame)
        ));
    }

    fn framed(tag: u8, id: &str) -> Vec<u8> {
        encode_envelope(tag, &encode_id_prefix(id).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn async_framing_round_trip_and_eof() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&framed(FRAME_TAG_REQUEST_STARTED, "q-1"));
        buf.extend_from_slice(&framed(FRAME_TAG_REQUEST_COMPLETED, "q-1"));

        let mut cursor = std::io::Cursor::new(buf);
        let p1 = read_length_prefixed(&mut cursor).await.unwrap().unwrap();
        assert_eq!(
            RawFrame::from_payload(&p1).unwrap().tag,
            FRAME_TAG_REQUEST_STARTED
        );
        let p2 = read_length_prefixed(&mut cursor).await.unwrap().unwrap();
        assert_eq!(
            RawFrame::from_payload(&p2).unwrap().tag,
            FRAME_TAG_REQUEST_COMPLETED
        );
        assert!(read_length_prefixed(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn async_write_then_read() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let payload =
            RawFrame::new(FRAME_TAG_PONG, encode_id_prefix("ping-1").unwrap()).to_payload();
        write_length_prefixed(&mut a, &payload).await.unwrap();
        let got = read_length_prefixed(&mut b).await.unwrap().unwrap();
        let frame = RawFrame::from_payload(&got).unwrap();
        assert_eq!(frame.tag, FRAME_TAG_PONG);
        assert_eq!(frame.id().as_deref(), Some("ping-1"));
    }

    // -------------------------------------------------------------------
    // EngineClient demux / terminal / deadline / cancellation
    // -------------------------------------------------------------------

    /// Scripted transport: yields queued inbound frames in order, records sent
    /// frames, and returns `None` (EOF) once drained.
    struct MockTransport {
        inbound: VecDeque<RawFrame>,
        sent: Vec<RawFrame>,
    }

    impl MockTransport {
        fn new(inbound: Vec<RawFrame>) -> Self {
            Self {
                inbound: inbound.into(),
                sent: Vec::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl FramedTransport for MockTransport {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            Ok(self.inbound.pop_front())
        }
        async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
            self.sent.push(frame.clone());
            Ok(())
        }
    }

    fn frame(tag: u8, id: &str) -> RawFrame {
        RawFrame::new(tag, encode_id_prefix(id).unwrap())
    }

    #[tokio::test]
    async fn client_sends_and_streams_to_terminal() {
        let transport = MockTransport::new(vec![
            frame(FRAME_TAG_REQUEST_STARTED, "req-1"),
            frame(FRAME_TAG_ROWS_BATCH, "req-1"),
            frame(FRAME_TAG_REQUEST_COMPLETED, "req-1"),
        ]);
        let mut client = EngineClient::new(transport);
        let id = client.next_request_id();
        assert_eq!(id, "req-1");
        client
            .send_request(REQUEST_TAG_EXECUTE_QUERY, encode_id_prefix(&id).unwrap())
            .await
            .unwrap();

        let frames = client.collect_response(&id, None).await.unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames.last().unwrap().tag, FRAME_TAG_REQUEST_COMPLETED);
        // After terminal, the stream is closed.
        assert!(client.next_frame(&id, None).await.unwrap().is_none());
        assert_eq!(client.transport_mut().sent.len(), 1);
    }

    #[tokio::test]
    async fn client_demultiplexes_interleaved_ids() {
        let transport = MockTransport::new(vec![
            frame(FRAME_TAG_REQUEST_STARTED, "req-2"), // for the *other* request
            frame(FRAME_TAG_REQUEST_STARTED, "req-1"),
            frame(FRAME_TAG_REQUEST_FAILED, "req-2"),
            frame(FRAME_TAG_REQUEST_COMPLETED, "req-1"),
        ]);
        let mut client = EngineClient::new(transport);

        // Await req-1 first: the two req-2 frames must be buffered, not lost.
        let one = client.collect_response("req-1", None).await.unwrap();
        assert_eq!(one.len(), 2);
        assert_eq!(one.last().unwrap().tag, FRAME_TAG_REQUEST_COMPLETED);

        let two = client.collect_response("req-2", None).await.unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two.last().unwrap().tag, FRAME_TAG_REQUEST_FAILED);
    }

    #[tokio::test]
    async fn pong_terminates_ping() {
        let transport = MockTransport::new(vec![frame(FRAME_TAG_PONG, "ping-1")]);
        let mut client = EngineClient::new(transport);
        let frames = client.collect_response("ping-1", None).await.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].tag, FRAME_TAG_PONG);
    }

    #[tokio::test]
    async fn transport_close_before_terminal_errors() {
        let transport = MockTransport::new(vec![frame(FRAME_TAG_REQUEST_STARTED, "req-1")]);
        let mut client = EngineClient::new(transport);
        let err = client.collect_response("req-1", None).await.unwrap_err();
        assert!(matches!(err, EngineClientError::TransportClosed(id) if id == "req-1"));
    }

    struct Never;
    #[async_trait::async_trait]
    impl FramedTransport for Never {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            std::future::pending().await
        }
        async fn send(&mut self, _f: &RawFrame) -> Result<(), ProtocolError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn deadline_times_out() {
        let mut client = EngineClient::new(Never);
        let deadline = Instant::now() + Duration::from_millis(20);
        let err = client
            .next_frame("req-1", Some(deadline))
            .await
            .unwrap_err();
        assert!(matches!(err, EngineClientError::Timeout(id) if id == "req-1"));
    }

    #[tokio::test]
    async fn cancellation_wins_the_race() {
        let mut client = EngineClient::new(Never);
        let cancel = async {};
        let err = client
            .next_frame_cancellable("req-1", None, cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, EngineClientError::Cancelled(id) if id == "req-1"));
    }
}
