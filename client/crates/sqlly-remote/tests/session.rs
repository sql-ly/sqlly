//! End-to-end session tests: an initiator and a responder wired through a fake
//! in-memory relay, all over `tokio::io::duplex` — no socket or TLS.
//!
//! The fake relay parses only the frame *header* (mirroring the real relay's
//! zero-knowledge behavior), routes by the clear header, and forwards the
//! opaque body. A pluggable mutator lets a test model a malicious/faulty relay
//! (bit-flip, metadata rewrite) so the endpoint-side integrity checks are
//! exercised over the wire.

use sqlly_remote::engine_protocol::{FRAME_TAG_ROWS_BATCH, RawFrame, encode_id_prefix};
use sqlly_remote::noise::{NoiseError, StaticKeypair};
use sqlly_remote::transport::{PeerSession, RemoteError, Role, SessionConfig};
use sqlly_wire::FrameKind;
use tokio::io::DuplexStream;
use uuid::Uuid;

/// How the fake relay should tamper with `Data` frames as it forwards them.
#[derive(Clone, Copy)]
enum Mutate {
    /// Faithful forward.
    None,
    /// Flip a bit in the first body byte of the first `Data` frame.
    FlipFirstDataByte,
    /// Rewrite the clear `message_id` of `Data` frames.
    RewriteMessageId,
    /// Rewrite the clear `chunk_count` of `Data` frames.
    RewriteChunkCount,
}

/// Pump frames between the two endpoints, routing by the clear destination id
/// and forwarding the opaque body. Applies `mutate` to `Data` frames.
async fn run_relay(a: DuplexStream, b: DuplexStream, id_a: Uuid, id_b: Uuid, mutate: Mutate) {
    use sqlly_remote::transport::FramedStream;

    let mut fa = FramedStream::new(a);
    let mut fb = FramedStream::new(b);
    let mut flipped = false;

    loop {
        tokio::select! {
            r = fa.recv() => {
                match r {
                    Ok(Some(frame)) => {
                        let frame = maybe_mutate(frame, mutate, &mut flipped);
                        // From endpoint A: destined for B.
                        assert_eq!(frame.destination, id_b);
                        assert_eq!(frame.source, id_a);
                        if fb.send(&frame).await.is_err() { break; }
                    }
                    _ => break,
                }
            }
            r = fb.recv() => {
                match r {
                    Ok(Some(frame)) => {
                        let frame = maybe_mutate(frame, mutate, &mut flipped);
                        assert_eq!(frame.destination, id_a);
                        assert_eq!(frame.source, id_b);
                        if fa.send(&frame).await.is_err() { break; }
                    }
                    _ => break,
                }
            }
        }
    }
}

fn maybe_mutate(
    mut frame: sqlly_wire::RelayFrame,
    mutate: Mutate,
    flipped: &mut bool,
) -> sqlly_wire::RelayFrame {
    if frame.kind != FrameKind::Data {
        return frame;
    }
    match mutate {
        Mutate::None => {}
        Mutate::FlipFirstDataByte => {
            if !*flipped {
                let mut body = frame.body.to_vec();
                if !body.is_empty() {
                    body[0] ^= 0x01;
                }
                frame.body = bytes::Bytes::from(body);
                *flipped = true;
            }
        }
        Mutate::RewriteMessageId => {
            frame.message_id = frame.message_id.wrapping_add(1000);
        }
        Mutate::RewriteChunkCount => {
            frame.chunk_count = frame.chunk_count.wrapping_add(7);
        }
    }
    frame
}

struct Peers {
    init: PeerSession<DuplexStream>,
    resp: PeerSession<DuplexStream>,
}

/// Stand up an initiator + responder through a relay with the given mutator and
/// (optionally) a wrong pin for one side.
async fn connect(
    mutate: Mutate,
    init_pins: Option<[u8; 32]>,
    resp_pins: Option<[u8; 32]>,
) -> std::result::Result<Peers, (Option<RemoteError>, Option<RemoteError>)> {
    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let init_pub = init_kp.public_key();
    let resp_pub = resp_kp.public_key();

    let init_id = Uuid::from_u128(0xA);
    let resp_id = Uuid::from_u128(0xB);

    // init <-> relay_a ; resp <-> relay_b
    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);

    tokio::spawn(run_relay(relay_a, relay_b, init_id, resp_id, mutate));

    let cfg = SessionConfig::default();
    let init_pin = init_pins.unwrap_or(resp_pub);
    let resp_pin = resp_pins.unwrap_or(init_pub);

    let init_fut = PeerSession::establish(
        Role::Initiator,
        init_side,
        &init_kp,
        &init_pin,
        init_id,
        resp_id,
        cfg,
    );
    let resp_fut = PeerSession::establish(
        Role::Responder,
        resp_side,
        &resp_kp,
        &resp_pin,
        resp_id,
        init_id,
        cfg,
    );

    let (init_res, resp_res) = tokio::join!(init_fut, resp_fut);
    match (init_res, resp_res) {
        (Ok(init), Ok(resp)) => Ok(Peers { init, resp }),
        (i, r) => Err((i.err(), r.err())),
    }
}

fn engine_frame(id: &str, payload: &[u8]) -> RawFrame {
    let mut body = encode_id_prefix(id).unwrap();
    body.extend_from_slice(payload);
    RawFrame::new(FRAME_TAG_ROWS_BATCH, body)
}

#[tokio::test]
async fn handshake_and_pin_check_then_single_chunk_round_trip() {
    let mut peers = connect(Mutate::None, None, None).await.expect("handshake");

    // Pin check succeeded on both sides.
    assert_ne!(peers.init.peer_static_key(), [0u8; 32]);

    let frame = engine_frame("req-1", b"small");
    peers.init.send_engine_frame(&frame).await.unwrap();
    let got = peers.resp.next_engine_frame().await.unwrap().unwrap();
    assert_eq!(got, frame);

    // And back the other way (responder -> initiator).
    let reply = engine_frame("req-1", b"answer");
    peers.resp.send_engine_frame(&reply).await.unwrap();
    let got = peers.init.next_engine_frame().await.unwrap().unwrap();
    assert_eq!(got, reply);
}

#[tokio::test]
async fn multi_chunk_round_trip() {
    let mut peers = connect(Mutate::None, None, None).await.expect("handshake");

    // A payload far larger than one chunk to force many Data frames.
    let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    let frame = engine_frame("req-big", &big);

    // Small chunk size to guarantee multiple chunks.
    peers.init.send_engine_frame(&frame).await.unwrap();
    let got = peers.resp.next_engine_frame().await.unwrap().unwrap();
    assert_eq!(got, frame);
}

#[tokio::test]
async fn small_chunk_config_forces_multiple_chunks() {
    // Explicitly tiny chunk size via SessionConfig, exercised end to end.
    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let (init_pub, resp_pub) = (init_kp.public_key(), resp_kp.public_key());
    let init_id = Uuid::from_u128(0xA);
    let resp_id = Uuid::from_u128(0xB);

    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);
    tokio::spawn(run_relay(relay_a, relay_b, init_id, resp_id, Mutate::None));

    let cfg = SessionConfig {
        max_chunk_plaintext: 4,
        ..SessionConfig::default()
    };
    let (init, resp) = tokio::join!(
        PeerSession::establish(
            Role::Initiator,
            init_side,
            &init_kp,
            &resp_pub,
            init_id,
            resp_id,
            cfg
        ),
        PeerSession::establish(
            Role::Responder,
            resp_side,
            &resp_kp,
            &init_pub,
            resp_id,
            init_id,
            cfg
        ),
    );
    let mut init = init.unwrap();
    let mut resp = resp.unwrap();

    let frame = engine_frame("q", b"twenty-five bytes exactly");
    init.send_engine_frame(&frame).await.unwrap();
    let got = resp.next_engine_frame().await.unwrap().unwrap();
    assert_eq!(got, frame);
}

/// A config that asks for 256 KiB chunks — over the Noise 64 KB transport-message
/// ceiling — is clamped at establish, so an engine frame larger than one Noise
/// message still seals and round-trips (as multiple chunks) instead of killing
/// the session.
#[tokio::test]
async fn oversized_chunk_config_is_clamped_and_large_frame_round_trips() {
    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let (init_pub, resp_pub) = (init_kp.public_key(), resp_kp.public_key());
    let init_id = Uuid::from_u128(0xA);
    let resp_id = Uuid::from_u128(0xB);

    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);
    tokio::spawn(run_relay(relay_a, relay_b, init_id, resp_id, Mutate::None));

    let cfg = SessionConfig {
        max_chunk_plaintext: 256 * 1024,
        ..SessionConfig::default()
    };
    assert_eq!(
        cfg.effective_max_chunk_plaintext(),
        sqlly_remote::transport::MAX_CHUNK_PLAINTEXT_LIMIT
    );
    let (init, resp) = tokio::join!(
        PeerSession::establish(
            Role::Initiator,
            init_side,
            &init_kp,
            &resp_pub,
            init_id,
            resp_id,
            cfg
        ),
        PeerSession::establish(
            Role::Responder,
            resp_side,
            &resp_kp,
            &init_pub,
            resp_id,
            init_id,
            cfg
        ),
    );
    let mut init = init.unwrap();
    let mut resp = resp.unwrap();

    // 200 KB payload: unsealable as a single chunk, fine once clamped. Send and
    // receive concurrently — four ~64 KB chunks exceed the duplex buffering, so
    // a sequential send-then-recv would deadlock on backpressure.
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let frame = engine_frame("req-clamped", &big);
    let (sent, got) = tokio::join!(init.send_engine_frame(&frame), resp.next_engine_frame());
    sent.unwrap();
    assert_eq!(got.unwrap().unwrap(), frame);
}

#[tokio::test]
async fn tamper_rejection() {
    let mut peers = connect(Mutate::FlipFirstDataByte, None, None)
        .await
        .expect("handshake");
    let frame = engine_frame("req-1", b"payload-bytes");
    peers.init.send_engine_frame(&frame).await.unwrap();
    let err = peers.resp.next_engine_frame().await.unwrap_err();
    assert!(matches!(err, RemoteError::Noise(_)), "got {err:?}");
}

#[tokio::test]
async fn metadata_mismatch_rejection() {
    let mut peers = connect(Mutate::RewriteMessageId, None, None)
        .await
        .expect("handshake");
    let frame = engine_frame("req-1", b"payload-bytes");
    peers.init.send_engine_frame(&frame).await.unwrap();
    let err = peers.resp.next_engine_frame().await.unwrap_err();
    assert!(
        matches!(err, RemoteError::Noise(NoiseError::MetadataMismatch)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn chunk_count_rewrite_rejection() {
    // Force multiple chunks so the outer chunk_count is meaningful.
    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let (init_pub, resp_pub) = (init_kp.public_key(), resp_kp.public_key());
    let init_id = Uuid::from_u128(0xA);
    let resp_id = Uuid::from_u128(0xB);

    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);
    tokio::spawn(run_relay(
        relay_a,
        relay_b,
        init_id,
        resp_id,
        Mutate::RewriteChunkCount,
    ));

    let cfg = SessionConfig {
        max_chunk_plaintext: 4,
        ..SessionConfig::default()
    };
    let (init, resp) = tokio::join!(
        PeerSession::establish(
            Role::Initiator,
            init_side,
            &init_kp,
            &resp_pub,
            init_id,
            resp_id,
            cfg
        ),
        PeerSession::establish(
            Role::Responder,
            resp_side,
            &resp_kp,
            &init_pub,
            resp_id,
            init_id,
            cfg
        ),
    );
    let mut init = init.unwrap();
    let mut resp = resp.unwrap();

    let frame = engine_frame("q", b"multi-chunk payload here");
    init.send_engine_frame(&frame).await.unwrap();
    let err = resp.next_engine_frame().await.unwrap_err();
    assert!(
        matches!(err, RemoteError::ChunkCountMismatch { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn peer_pin_mismatch_rejection() {
    // Initiator pins a wrong key for the responder.
    let wrong = [0x42u8; 32];
    let outcome = connect(Mutate::None, Some(wrong), None).await;
    let (init_err, _resp_err) = outcome.err().expect("establish should fail");
    assert!(
        matches!(
            init_err,
            Some(RemoteError::Noise(NoiseError::PeerPinMismatch))
        ),
        "got {init_err:?}"
    );
}

/// The multi-peer responder (the bridge's establishment path): the session must
/// pin whichever AUTHORIZED device initiates — including one that is not first
/// in the list. Locking to the first entry is the cross-machine bug where only
/// the first-paired app device could ever connect.
#[tokio::test]
async fn responder_accepts_any_authorized_peer_not_just_the_first() {
    use sqlly_remote::transport::AuthorizedPeer;

    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let resp_pub = resp_kp.public_key();

    let first_device = Uuid::from_u128(0xA1);
    let second_device = Uuid::from_u128(0xA2);
    let resp_id = Uuid::from_u128(0xB);

    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);
    // The initiator is the SECOND authorized device.
    tokio::spawn(run_relay(
        relay_a,
        relay_b,
        second_device,
        resp_id,
        Mutate::None,
    ));

    let authorized = [
        AuthorizedPeer {
            id: first_device,
            static_key: [0x55u8; 32], // some other device's key
        },
        AuthorizedPeer {
            id: second_device,
            static_key: init_kp.public_key(),
        },
    ];

    let cfg = SessionConfig::default();
    let (init, resp) = tokio::join!(
        PeerSession::establish(
            Role::Initiator,
            init_side,
            &init_kp,
            &resp_pub,
            second_device,
            resp_id,
            cfg
        ),
        PeerSession::establish_responder(resp_side, &resp_kp, &authorized, resp_id, cfg),
    );
    let mut init = init.expect("initiator handshake");
    let mut resp = resp.expect("responder handshake with the second authorized device");

    // The session pinned the device that actually connected.
    assert_eq!(resp.peer_id(), second_device);
    assert_eq!(resp.peer_static_key(), init_kp.public_key());

    let frame = engine_frame("req-1", b"from the second device");
    init.send_engine_frame(&frame).await.unwrap();
    let got = resp.next_engine_frame().await.unwrap().unwrap();
    assert_eq!(got, frame);
}

/// An initiator whose wire id is not in the authorized set is rejected before
/// the responder replies to message 1.
#[tokio::test]
async fn responder_rejects_unauthorized_peer() {
    use sqlly_remote::transport::AuthorizedPeer;

    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let resp_pub = resp_kp.public_key();

    let stranger = Uuid::from_u128(0xEE);
    let resp_id = Uuid::from_u128(0xB);

    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);
    tokio::spawn(run_relay(relay_a, relay_b, stranger, resp_id, Mutate::None));

    let authorized = [AuthorizedPeer {
        id: Uuid::from_u128(0xA1),
        static_key: [0x55u8; 32],
    }];

    let cfg = SessionConfig::default();
    let (_init, resp) = tokio::join!(
        PeerSession::establish(
            Role::Initiator,
            init_side,
            &init_kp,
            &resp_pub,
            stranger,
            resp_id,
            cfg
        ),
        PeerSession::establish_responder(resp_side, &resp_kp, &authorized, resp_id, cfg),
    );
    let err = resp.err().expect("responder must reject the stranger");
    assert!(matches!(err, RemoteError::PeerMismatch), "got {err:?}");
}

/// Claiming an authorized wire id is not enough: the initiator's static key
/// from message 3 must match the key pinned for that id, or the responder's
/// handshake fails cryptographically.
#[tokio::test]
async fn responder_rejects_key_mismatch_for_claimed_id() {
    use sqlly_remote::transport::AuthorizedPeer;

    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let resp_pub = resp_kp.public_key();

    let claimed = Uuid::from_u128(0xA1);
    let resp_id = Uuid::from_u128(0xB);

    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);
    tokio::spawn(run_relay(relay_a, relay_b, claimed, resp_id, Mutate::None));

    // The authorized entry for `claimed` pins a DIFFERENT key than the one the
    // initiator actually holds.
    let authorized = [AuthorizedPeer {
        id: claimed,
        static_key: [0x77u8; 32],
    }];

    let cfg = SessionConfig::default();
    let (_init, resp) = tokio::join!(
        PeerSession::establish(
            Role::Initiator,
            init_side,
            &init_kp,
            &resp_pub,
            claimed,
            resp_id,
            cfg
        ),
        PeerSession::establish_responder(resp_side, &resp_kp, &authorized, resp_id, cfg),
    );
    let err = resp.err().expect("responder must reject the imposter");
    assert!(
        matches!(err, RemoteError::Noise(NoiseError::PeerPinMismatch)),
        "got {err:?}"
    );
}

/// The relay can forward the initiator's handshake message 1 right behind the
/// responder's Auth ack, so one read may buffer both. Handing the framed stream
/// (not `into_inner()`) to the session must keep message 1, or the handshake
/// hangs waiting for a frame already read (build 14004).
#[tokio::test]
async fn handshake_frame_buffered_behind_auth_ack_is_not_lost() {
    use sqlly_remote::transport::{FramedStream, authenticate};
    use sqlly_wire::RelayFrame;
    use tokio::io::AsyncWriteExt;

    let init_kp = StaticKeypair::generate().unwrap();
    let resp_kp = StaticKeypair::generate().unwrap();
    let init_pub = init_kp.public_key();
    let resp_pub = resp_kp.public_key();
    let init_id = Uuid::from_u128(0xA);
    let resp_id = Uuid::from_u128(0xB);
    let relay_id = Uuid::from_u128(0xC);

    let (init_side, relay_a) = tokio::io::duplex(64 * 1024);
    let (resp_side, relay_b) = tokio::io::duplex(64 * 1024);

    tokio::spawn(async move {
        let mut fa = FramedStream::new(relay_a);
        let mut fb = FramedStream::new(relay_b);
        let auth = fb.recv().await.unwrap().unwrap();
        assert_eq!(auth.kind, FrameKind::Auth);
        let m1 = fa.recv().await.unwrap().unwrap();
        // Ack and message 1 in a single write, so the responder reads both at once.
        let mut bytes = RelayFrame::new(FrameKind::Auth, relay_id, resp_id, 0, bytes::Bytes::new())
            .encode()
            .unwrap()
            .to_vec();
        bytes.extend_from_slice(&m1.encode().unwrap());
        let mut b = fb.into_inner();
        b.write_all(&bytes).await.unwrap();
        run_relay(fa.into_inner(), b, init_id, resp_id, Mutate::None).await;
    });

    let cfg = SessionConfig::default();
    let init_fut = PeerSession::establish(
        Role::Initiator,
        init_side,
        &init_kp,
        &resp_pub,
        init_id,
        resp_id,
        cfg,
    );
    let resp_fut = async {
        let mut framed = FramedStream::new(resp_side);
        authenticate(&mut framed, resp_id, relay_id, b"token").await?;
        PeerSession::establish_framed(
            Role::Responder,
            framed,
            &resp_kp,
            &init_pub,
            resp_id,
            init_id,
            cfg,
        )
        .await
    };

    let (init_res, resp_res) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(init_fut, resp_fut)
    })
    .await
    .expect("handshake hung: message 1 was dropped with the auth read buffer");
    init_res.expect("initiator handshake");
    resp_res.expect("responder handshake");
}
