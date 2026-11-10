//! End-to-end tests for `sqlly-noise`: identity/keyring, XX handshake, peer
//! pinning, per-chunk transport encryption, tamper/wrong-session/metadata
//! rejection, and private-key redaction.

use bytes::Bytes;
use sqlly_remote::noise::identity::SecretStore;
use sqlly_remote::noise::{
    Handshake, InMemorySecretStore, InnerChunk, NoiseError, OuterMeta, StaticKeypair, Transport,
};
use uuid::Uuid;

/// Drive a full three-message XX handshake and return the two pinned
/// transports (initiator, responder).
fn established_pair(
    initiator: &StaticKeypair,
    responder: &StaticKeypair,
) -> (Transport, Transport) {
    let mut hi = Handshake::initiator(initiator).unwrap();
    let mut hr = Handshake::responder(responder).unwrap();

    // 1. -> e
    let m1 = hi.write_message(&[]).unwrap();
    let _ = hr.read_message(&m1).unwrap();
    // 2. <- e, ee, s, es
    let m2 = hr.write_message(&[]).unwrap();
    let _ = hi.read_message(&m2).unwrap();
    // 3. -> s, se
    let m3 = hi.write_message(&[]).unwrap();
    let _ = hr.read_message(&m3).unwrap();

    assert!(hi.is_finished());
    assert!(hr.is_finished());

    let ti = hi.into_transport_pinned(&responder.public_key()).unwrap();
    let tr = hr.into_transport_pinned(&initiator.public_key()).unwrap();
    (ti, tr)
}

fn sample_inner(payload: &[u8]) -> InnerChunk {
    InnerChunk {
        source: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
        destination: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
        message_id: 42,
        chunk_index: 3,
        flags: 0x05,
        payload: Bytes::copy_from_slice(payload),
    }
}

fn outer_of(inner: &InnerChunk) -> OuterMeta {
    OuterMeta {
        source: inner.source,
        destination: inner.destination,
        message_id: inner.message_id,
        chunk_index: inner.chunk_index,
        flags: inner.flags,
    }
}

// ---------------------------------------------------------------------------
// Identity / keyring
// ---------------------------------------------------------------------------

#[test]
fn generate_produces_distinct_keypairs() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    assert_ne!(a.public_key(), b.public_key());
    assert_eq!(a.public_key().len(), 32);
}

#[test]
fn load_or_generate_generates_on_first_run_then_loads() {
    let store = InMemorySecretStore::new();
    assert!(store.is_empty());

    let first = StaticKeypair::load_or_generate_with(&store, "bridge", "device-abc").unwrap();
    // Persisted exactly one 32-byte secret.
    assert_eq!(store.len(), 1);
    let stored = store
        .load("bridge\u{0}device-abc")
        .unwrap()
        .expect("secret persisted");
    assert_eq!(stored.len(), 32);

    // Second call loads and derives the same public key + fingerprint. This
    // also proves x25519 public-key derivation matches snow's generation.
    let second = StaticKeypair::load_or_generate_with(&store, "bridge", "device-abc").unwrap();
    assert_eq!(first.public_key(), second.public_key());
    assert_eq!(first.fingerprint(), second.fingerprint());
    assert_eq!(store.len(), 1, "no new secret written on load");
}

#[test]
fn scope_and_client_id_isolate_identities() {
    let store = InMemorySecretStore::new();
    let a = StaticKeypair::load_or_generate_with(&store, "bridge", "dev-1").unwrap();
    let b = StaticKeypair::load_or_generate_with(&store, "bridge", "dev-2").unwrap();
    let c = StaticKeypair::load_or_generate_with(&store, "app", "dev-1").unwrap();
    assert_ne!(a.public_key(), b.public_key());
    assert_ne!(a.public_key(), c.public_key());
    assert_eq!(store.len(), 3);
}

#[test]
fn no_entry_is_handled_gracefully() {
    let store = InMemorySecretStore::new();
    // Fresh store => load returns None (not an error) => generation happens.
    assert_eq!(store.load("bridge\u{0}dev").unwrap(), None);
    let kp = StaticKeypair::load_or_generate_with(&store, "bridge", "dev").unwrap();
    assert_eq!(kp.public_key().len(), 32);

    // After delete, a subsequent load_or_generate mints a fresh identity.
    store.delete("bridge\u{0}dev").unwrap();
    assert!(store.is_empty());
    let kp2 = StaticKeypair::load_or_generate_with(&store, "bridge", "dev").unwrap();
    assert_ne!(kp.public_key(), kp2.public_key());
}

#[test]
fn invalid_stored_length_is_rejected() {
    let store = InMemorySecretStore::new();
    store.store("bridge\u{0}bad", &[0u8; 16]).unwrap();
    let err = StaticKeypair::load_or_generate_with(&store, "bridge", "bad").unwrap_err();
    assert!(matches!(err, NoiseError::InvalidKeyLength));
}

#[test]
fn fingerprint_is_stable_hex() {
    let kp = StaticKeypair::from_private_bytes(&[9u8; 32]).unwrap();
    let fp = kp.fingerprint();
    // BLAKE2s-256 => 32 bytes => 64 lowercase hex chars.
    assert_eq!(fp.len(), 64);
    assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(fp, kp.fingerprint(), "deterministic");
}

#[test]
fn private_key_never_appears_in_debug() {
    let secret = [7u8; 32];
    let secret_hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();

    let kp = StaticKeypair::from_private_bytes(&secret).unwrap();
    let dbg = format!("{kp:?}");
    assert!(
        !dbg.contains(&secret_hex),
        "private key leaked in Debug: {dbg}"
    );
    assert!(dbg.contains("redacted"));
    // Public key IS expected to be present.
    let pub_hex: String = kp.public_key().iter().map(|b| format!("{b:02x}")).collect();
    assert!(dbg.contains(&pub_hex));
}

#[test]
fn errors_do_not_leak_key_material() {
    let secret = [3u8; 32];
    let secret_hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    for err in [
        NoiseError::PeerPinMismatch,
        NoiseError::MetadataMismatch,
        NoiseError::HandshakeIncomplete,
        NoiseError::InvalidKeyLength,
        NoiseError::Snow("bad".into()),
    ] {
        let s = format!("{err}") + &format!("{err:?}");
        assert!(!s.contains(&secret_hex));
    }
}

// ---------------------------------------------------------------------------
// Handshake + pinning
// ---------------------------------------------------------------------------

#[test]
fn full_xx_handshake_and_round_trip() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let (mut ti, mut tr) = established_pair(&a, &b);

    // Initiator -> responder.
    let inner = sample_inner(b"hello engine frame");
    let ct = ti.seal(&inner).unwrap();
    assert_ne!(
        ct.as_slice(),
        inner.payload.as_ref(),
        "payload is encrypted"
    );
    let got = tr.open(&ct).unwrap();
    assert_eq!(got, inner);

    // Responder -> initiator (full duplex).
    let back = sample_inner(b"response frame");
    let ctb = tr.seal(&back).unwrap();
    let gotb = ti.open(&ctb).unwrap();
    assert_eq!(gotb, back);
}

#[test]
fn empty_payload_round_trips() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let (mut ti, mut tr) = established_pair(&a, &b);
    let inner = sample_inner(b"");
    let ct = ti.seal(&inner).unwrap();
    let got = tr.open(&ct).unwrap();
    assert_eq!(got.payload.len(), 0);
    assert_eq!(got, inner);
}

#[test]
fn peer_pin_mismatch_is_rejected() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let imposter = StaticKeypair::generate().unwrap();

    let mut hi = Handshake::initiator(&a).unwrap();
    let mut hr = Handshake::responder(&b).unwrap();
    let m1 = hi.write_message(&[]).unwrap();
    hr.read_message(&m1).unwrap();
    let m2 = hr.write_message(&[]).unwrap();
    hi.read_message(&m2).unwrap();
    let m3 = hi.write_message(&[]).unwrap();
    hr.read_message(&m3).unwrap();

    // Initiator learned b's real static; pinning against the imposter fails.
    assert_eq!(hi.remote_static(), Some(b.public_key()));
    let err = hi
        .into_transport_pinned(&imposter.public_key())
        .unwrap_err();
    assert!(matches!(err, NoiseError::PeerPinMismatch));

    // Pinning against the correct key succeeds.
    assert!(hr.into_transport_pinned(&a.public_key()).is_ok());
}

#[test]
fn transport_before_handshake_is_rejected() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let hi = Handshake::initiator(&a).unwrap();
    let err = hi.into_transport_pinned(&b.public_key()).unwrap_err();
    assert!(matches!(err, NoiseError::HandshakeIncomplete));
}

// ---------------------------------------------------------------------------
// Transport security
// ---------------------------------------------------------------------------

#[test]
fn tampered_ciphertext_fails_to_decrypt() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let (mut ti, mut tr) = established_pair(&a, &b);

    let inner = sample_inner(b"sensitive rows");
    let mut ct = ti.seal(&inner).unwrap();
    // Flip a bit in the ciphertext body.
    ct[5] ^= 0x01;
    let err = tr.open(&ct).unwrap_err();
    assert!(matches!(err, NoiseError::Snow(_)), "got {err:?}");
}

#[test]
fn wrong_session_key_is_rejected() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let c = StaticKeypair::generate().unwrap();
    let d = StaticKeypair::generate().unwrap();

    // Two independent sessions => independent session keys.
    let (mut ti_ab, _tr_ab) = established_pair(&a, &b);
    let (_ti_cd, mut tr_cd) = established_pair(&c, &d);

    let inner = sample_inner(b"cross-session");
    let ct = ti_ab.seal(&inner).unwrap();
    // Decrypting session A/B's ciphertext with session C/D's transport fails.
    let err = tr_cd.open(&ct).unwrap_err();
    assert!(matches!(err, NoiseError::Snow(_)), "got {err:?}");
}

#[test]
fn inner_metadata_matching_outer_verifies() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let (mut ti, mut tr) = established_pair(&a, &b);

    let inner = sample_inner(b"routed payload");
    let ct = ti.seal(&inner).unwrap();
    // Honest relay: outer header matches the sealed inner copy.
    let payload = tr.open_verified(&ct, &outer_of(&inner)).unwrap();
    assert_eq!(payload, inner.payload);
}

#[test]
fn inner_metadata_mismatch_is_rejected() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let (mut ti, mut tr) = established_pair(&a, &b);

    let inner = sample_inner(b"routed payload");
    let ct = ti.seal(&inner).unwrap();

    // A relay that rewrote the destination in the clear outer header is
    // detected: decryption still succeeds, but the authenticated inner copy no
    // longer matches the outer frame.
    let tampered_outer = OuterMeta {
        destination: Uuid::from_u128(0xDEAD_BEEF),
        ..outer_of(&inner)
    };
    let err = tr.open_verified(&ct, &tampered_outer).unwrap_err();
    assert!(matches!(err, NoiseError::MetadataMismatch));
}

#[test]
fn decoded_inner_metadata_is_exposed_for_comparison() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let (mut ti, mut tr) = established_pair(&a, &b);

    let inner = sample_inner(b"payload");
    let ct = ti.seal(&inner).unwrap();
    let decoded = tr.open(&ct).unwrap();
    // Caller can compare each field against the outer frame header.
    assert_eq!(decoded.source, inner.source);
    assert_eq!(decoded.destination, inner.destination);
    assert_eq!(decoded.message_id, inner.message_id);
    assert_eq!(decoded.chunk_index, inner.chunk_index);
    assert_eq!(decoded.flags, inner.flags);
    assert!(decoded.matches_outer(&outer_of(&inner)));
}

// ---------------------------------------------------------------------------
// Malformed handshake bytes: truncated, garbage, bit-flipped, and oversized
// inputs into read_message must all fail typed — never panic, never complete
// a session with an attacker in the middle.
// ---------------------------------------------------------------------------

#[test]
fn truncated_handshake_messages_fail_typed() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();
    let mut initiator = Handshake::initiator(&a).unwrap();
    let msg1 = initiator.write_message(&[]).unwrap();

    for cut in [0, 1, msg1.len() / 2, msg1.len().saturating_sub(1)] {
        let mut responder = Handshake::responder(&b).unwrap();
        let result = responder.read_message(&msg1[..cut]);
        assert!(result.is_err(), "cut at {cut} must fail, got {result:?}");
        assert!(!responder.is_finished());
    }
}

#[test]
fn garbage_and_bitflipped_handshake_messages_fail_typed() {
    let a = StaticKeypair::generate().unwrap();
    let b = StaticKeypair::generate().unwrap();

    // Pure garbage. Message 1 of XX is an unauthenticated ephemeral key plus
    // a plaintext payload, so any ≥32 bytes PARSE by design — the protocol
    // authenticates at messages 2/3. The security property is therefore:
    // too-short garbage errors typed, and parseable garbage can never yield a
    // finished handshake or a transport.
    for size in [0usize, 1, 16, 31] {
        let mut responder = Handshake::responder(&b).unwrap();
        let garbage = vec![0xA5u8; size];
        assert!(
            responder.read_message(&garbage).is_err(),
            "garbage of {size} bytes must fail"
        );
        assert!(!responder.is_finished());
    }
    for size in [32usize, 48, 96, 1024] {
        let mut responder = Handshake::responder(&b).unwrap();
        let garbage = vec![0xA5u8; size];
        let _ = responder.read_message(&garbage);
        assert!(
            !responder.is_finished(),
            "garbage of {size} bytes must never finish a handshake"
        );
    }

    // A real message with every byte position flipped, one at a time (bounded
    // to the first 64 positions to keep the test fast).
    let mut initiator = Handshake::initiator(&a).unwrap();
    let msg1 = initiator.write_message(&[]).unwrap();
    for i in 0..msg1.len().min(64) {
        let mut mutated = msg1.clone();
        mutated[i] ^= 0x01;
        let mut responder = Handshake::responder(&b).unwrap();
        // Message 1 of XX carries a plaintext ephemeral key, so an early-byte
        // flip may pass message 1 — but the handshake must then fail at
        // message 3 (the DH results diverge), never complete against the
        // mutated transcript.
        let Ok(_) = responder.read_message(&mutated) else {
            continue; // failed immediately: exactly right
        };
        let msg2 = responder.write_message(&[]).unwrap();
        let Ok(_) = initiator.read_message(&msg2) else {
            initiator = Handshake::initiator(&a).unwrap();
            let _ = initiator.write_message(&[]).unwrap();
            continue;
        };
        let msg3 = initiator.write_message(&[]).unwrap();
        assert!(
            responder.read_message(&msg3).is_err(),
            "flip at byte {i} must not complete a handshake"
        );
        // Fresh initiator for the next iteration (this one advanced).
        initiator = Handshake::initiator(&a).unwrap();
        let _ = initiator.write_message(&[]).unwrap();
    }
}

#[test]
fn oversized_handshake_message_fails_without_excessive_allocation() {
    let b = StaticKeypair::generate().unwrap();
    let mut responder = Handshake::responder(&b).unwrap();
    // Far larger than any legal Noise handshake message (max 65535).
    let oversized = vec![0u8; 1 << 20];
    assert!(responder.read_message(&oversized).is_err());
}
