//! The role-agnostic peer session established *through* the relay.
//!
//! [`PeerSession`] is generic over any `AsyncRead + AsyncWrite` byte stream, so
//! it is exercised in tests over `tokio::io::duplex` with a fake in-memory relay
//! — no socket or TLS required. The same type serves both roles:
//!
//! * The **initiator** (the GPUI/CLI controlling client, Phase 3) drives an
//!   `EngineClient` through the [`FramedTransport`] impl.
//! * The **responder** (the on-prem bridge) uses [`PeerSession::next_engine_frame`]
//!   / [`PeerSession::send_engine_frame`] as its pump: read decrypted inbound
//!   engine frames, write outbound ones.
//!
//! A session performs the Noise XX handshake by exchanging
//! [`NoiseHandshake`](FrameKind::NoiseHandshake) frames with the peer via the
//! relay, then pins the peer's static key. Afterwards, engine frames are
//! chunked, sealed, and tunnelled as [`Data`](FrameKind::Data) frames.

use async_trait::async_trait;
use bytes::Bytes;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::engine_protocol::{FramedTransport, ProtocolError, RawFrame};
use crate::noise::{Handshake, KEY_LEN, StaticKeypair, Transport};
use sqlly_wire::{
    AssemblerConfig, ChunkAssembler, FrameKind, MAX_BODY_LEN, PROTOCOL_VERSION, RelayFrame,
};
use uuid::Uuid;

use crate::transport::error::{RemoteError, Result};
use crate::transport::framed::FramedStream;
use crate::transport::sealing::{open_data_frame, seal_engine_message};

/// Message id used for every Noise handshake frame (handshake messages are not
/// chunked application messages).
const HANDSHAKE_MESSAGE_ID: u64 = 0;

/// Hard ceiling on the plaintext chunk size. A Noise transport message
/// (ciphertext) is limited to 65,535 bytes *including* the 16-byte AEAD tag,
/// and sealing prepends a 45-byte authenticated inner header plus the 4-byte
/// bound `chunk_count`, so a plaintext chunk above this cannot be sealed at
/// all: `65_535 - 16 - 45 - 4 = 65_470`.
pub const MAX_CHUNK_PLAINTEXT_LIMIT: usize = 65_470;

/// Default plaintext chunk size for splitting engine frames before sealing.
///
/// Kept under [`MAX_CHUNK_PLAINTEXT_LIMIT`] with headroom (and far under
/// [`MAX_BODY_LEN`]) so that every sealed body fits one Noise transport
/// message and one relay frame.
pub const DEFAULT_MAX_CHUNK_PLAINTEXT: usize = 60 * 1024;

/// Which side of the Noise XX handshake this session drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Sends handshake message 1 and 3 (the controlling client).
    Initiator,
    /// Sends handshake message 2 (the on-prem bridge).
    Responder,
}

/// Tuning for a [`PeerSession`].
#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    /// Plaintext chunk size for outbound engine frames.
    pub max_chunk_plaintext: usize,
    /// Inbound reassembly bounds.
    pub assembler: AssemblerConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_chunk_plaintext: DEFAULT_MAX_CHUNK_PLAINTEXT,
            assembler: AssemblerConfig::default(),
        }
    }
}

impl SessionConfig {
    /// The chunk size sealing actually uses: `max_chunk_plaintext` clamped to
    /// [`MAX_CHUNK_PLAINTEXT_LIMIT`]. No configuration may exceed the Noise
    /// transport-message ceiling — an oversized chunk would fail to seal and
    /// kill the session.
    #[must_use]
    pub fn effective_max_chunk_plaintext(&self) -> usize {
        self.max_chunk_plaintext.min(MAX_CHUNK_PLAINTEXT_LIMIT)
    }
}

/// One identity a responder is willing to hand-shake with: the peer's wire id
/// and its pinned Noise static public key. A responder serving several
/// authorized controllers (the bridge, whose account may pair many app
/// devices) passes one of these per active pair to
/// [`PeerSession::establish_responder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizedPeer {
    /// The peer's authenticated wire id (relay frame `source`).
    pub id: Uuid,
    /// The peer's pinned Noise static public key.
    pub static_key: [u8; KEY_LEN],
}

/// An established, end-to-end-encrypted session with a peer, reached through the
/// relay. Owns the relay byte stream and the Noise transport cipher.
pub struct PeerSession<S> {
    framed: FramedStream<S>,
    transport: Transport,
    local_id: Uuid,
    peer_id: Uuid,
    peer_static: [u8; KEY_LEN],
    max_chunk: usize,
    next_message_id: u64,
    assembler: ChunkAssembler,
}

impl<S> PeerSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Run the Noise XX handshake over `stream` as `role`, verifying the peer's
    /// static key against `expected_peer`, and return the ready session.
    ///
    /// `local_id` is this endpoint's authenticated id and `peer_id` the pinned
    /// peer's id; both are used as the relay `source`/`destination` and are
    /// checked against every inbound frame.
    pub async fn establish(
        role: Role,
        stream: S,
        local_key: &StaticKeypair,
        expected_peer: &[u8; KEY_LEN],
        local_id: Uuid,
        peer_id: Uuid,
        config: SessionConfig,
    ) -> Result<Self> {
        Self::establish_framed(
            role,
            FramedStream::new(stream),
            local_key,
            expected_peer,
            local_id,
            peer_id,
            config,
        )
        .await
    }

    /// [`establish`](Self::establish) over the [`FramedStream`] that already
    /// carried the relay auth, keeping any frames it buffered past the Auth ack.
    pub async fn establish_framed(
        role: Role,
        mut framed: FramedStream<S>,
        local_key: &StaticKeypair,
        expected_peer: &[u8; KEY_LEN],
        local_id: Uuid,
        peer_id: Uuid,
        config: SessionConfig,
    ) -> Result<Self> {
        if role == Role::Responder {
            // The single-peer responder is the one-authorized-peer case of the
            // multi-peer establishment — one code path, identical checks.
            return Self::establish_responder_framed(
                framed,
                local_key,
                &[AuthorizedPeer {
                    id: peer_id,
                    static_key: *expected_peer,
                }],
                local_id,
                config,
            )
            .await;
        }

        let transport =
            Self::handshake_initiator(&mut framed, local_key, expected_peer, local_id, peer_id)
                .await?;

        Ok(Self {
            framed,
            transport,
            local_id,
            peer_id,
            peer_static: *expected_peer,
            max_chunk: config.effective_max_chunk_plaintext(),
            next_message_id: 1,
            assembler: ChunkAssembler::new(config.assembler),
        })
    }

    /// Run the Noise XX handshake as the **responder**, accepting whichever of
    /// the `authorized` peers initiates, and return the ready session pinned to
    /// that peer.
    ///
    /// The first handshake frame's `source` — which the relay has already
    /// verified against the sender's authenticated wire id — selects the
    /// candidate; an unlisted source is rejected with [`RemoteError::PeerMismatch`]
    /// before the responder reveals anything. The selection is then enforced
    /// cryptographically: the initiator's static key from message 3 must match
    /// the key pinned for the id it claimed, or the handshake fails.
    pub async fn establish_responder(
        stream: S,
        local_key: &StaticKeypair,
        authorized: &[AuthorizedPeer],
        local_id: Uuid,
        config: SessionConfig,
    ) -> Result<Self> {
        Self::establish_responder_framed(
            FramedStream::new(stream),
            local_key,
            authorized,
            local_id,
            config,
        )
        .await
    }

    /// [`establish_responder`](Self::establish_responder) over the
    /// [`FramedStream`] that already carried the relay auth, keeping any frames
    /// it buffered past the Auth ack.
    pub async fn establish_responder_framed(
        mut framed: FramedStream<S>,
        local_key: &StaticKeypair,
        authorized: &[AuthorizedPeer],
        local_id: Uuid,
        config: SessionConfig,
    ) -> Result<Self> {
        let mut hs = Handshake::responder(local_key)?;

        // <- e : the M1 source picks WHICH authorized peer this session is with.
        let (source, m1) = Self::recv_handshake_from_any(&mut framed, local_id).await?;
        let peer = authorized
            .iter()
            .find(|p| p.id == source)
            .copied()
            .ok_or(RemoteError::PeerMismatch)?;
        hs.read_message(&m1)?;
        // -> e, ee, s, es
        let m2 = hs.write_message(&[])?;
        Self::send_handshake(&mut framed, local_id, peer.id, m2).await?;
        // <- s, se
        let m3 = Self::recv_handshake(&mut framed, local_id, peer.id).await?;
        hs.read_message(&m3)?;
        let transport = hs.into_transport_pinned(&peer.static_key)?;

        Ok(Self {
            framed,
            transport,
            local_id,
            peer_id: peer.id,
            peer_static: peer.static_key,
            max_chunk: config.effective_max_chunk_plaintext(),
            next_message_id: 1,
            assembler: ChunkAssembler::new(config.assembler),
        })
    }

    async fn handshake_initiator(
        framed: &mut FramedStream<S>,
        local_key: &StaticKeypair,
        expected_peer: &[u8; KEY_LEN],
        local_id: Uuid,
        peer_id: Uuid,
    ) -> Result<Transport> {
        let mut hs = Handshake::initiator(local_key)?;
        // -> e
        let m1 = hs.write_message(&[])?;
        Self::send_handshake(framed, local_id, peer_id, m1).await?;
        // <- e, ee, s, es
        let m2 = Self::recv_handshake(framed, local_id, peer_id).await?;
        hs.read_message(&m2)?;
        // -> s, se
        let m3 = hs.write_message(&[])?;
        Self::send_handshake(framed, local_id, peer_id, m3).await?;
        Ok(hs.into_transport_pinned(expected_peer)?)
    }

    async fn send_handshake(
        framed: &mut FramedStream<S>,
        local_id: Uuid,
        peer_id: Uuid,
        body: Vec<u8>,
    ) -> Result<()> {
        let frame = RelayFrame::new(
            FrameKind::NoiseHandshake,
            local_id,
            peer_id,
            HANDSHAKE_MESSAGE_ID,
            Bytes::from(body),
        );
        framed.send(&frame).await
    }

    /// Receive one handshake frame addressed to us from ANY sender, returning
    /// the (relay-authenticated) source so the responder can select which
    /// authorized peer is initiating. Control frames and misrouting map to
    /// errors as in [`Self::recv_handshake`].
    async fn recv_handshake_from_any(
        framed: &mut FramedStream<S>,
        local_id: Uuid,
    ) -> Result<(Uuid, Bytes)> {
        match framed.recv().await? {
            None => Err(RemoteError::HandshakeClosed),
            Some(frame) => match frame.kind {
                FrameKind::NoiseHandshake => {
                    if frame.destination != local_id {
                        return Err(RemoteError::PeerMismatch);
                    }
                    Ok((frame.source, frame.body))
                }
                FrameKind::RelayError => Err(frame
                    .relay_error_code()
                    .map_or(RemoteError::HandshakeClosed, RemoteError::Relay)),
                other => Err(RemoteError::UnexpectedFrame(other)),
            },
        }
    }

    /// Receive one handshake frame, mapping control frames and misrouting to
    /// errors.
    async fn recv_handshake(
        framed: &mut FramedStream<S>,
        local_id: Uuid,
        peer_id: Uuid,
    ) -> Result<Bytes> {
        match framed.recv().await? {
            None => Err(RemoteError::HandshakeClosed),
            Some(frame) => match frame.kind {
                FrameKind::NoiseHandshake => {
                    if frame.source != peer_id || frame.destination != local_id {
                        return Err(RemoteError::PeerMismatch);
                    }
                    Ok(frame.body)
                }
                FrameKind::RelayError => Err(frame
                    .relay_error_code()
                    .map_or(RemoteError::HandshakeClosed, RemoteError::Relay)),
                other => Err(RemoteError::UnexpectedFrame(other)),
            },
        }
    }

    fn next_message_id(&mut self) -> u64 {
        let id = self.next_message_id;
        self.next_message_id += 1;
        id
    }

    /// Chunk, seal, and send one engine frame to the peer as `Data` frames.
    pub async fn send_engine_frame(&mut self, frame: &RawFrame) -> Result<()> {
        let message = frame.to_payload();
        let message_id = self.next_message_id();
        let frames = seal_engine_message(
            &mut self.transport,
            self.local_id,
            self.peer_id,
            message_id,
            &message,
            self.max_chunk,
        )?;
        for f in frames {
            self.framed.send(&f).await?;
        }
        Ok(())
    }

    /// Receive the next reassembled engine frame from the peer.
    ///
    /// Consumes and handles inbound control frames (`Ping` is answered with
    /// `Pong`, `Pong` is ignored) and returns:
    ///
    /// * `Ok(Some(frame))` when a complete engine frame has been reassembled,
    /// * `Ok(None)` when the peer closes the stream (EOF or a `Close` frame),
    /// * `Err(_)` on a decrypt/metadata/reassembly failure or a relay error.
    pub async fn next_engine_frame(&mut self) -> Result<Option<RawFrame>> {
        loop {
            let frame = match self.framed.recv().await? {
                None => return Ok(None),
                Some(frame) => frame,
            };

            match frame.kind {
                FrameKind::Data => {
                    if frame.source != self.peer_id || frame.destination != self.local_id {
                        return Err(RemoteError::PeerMismatch);
                    }
                    let chunk = open_data_frame(&mut self.transport, &frame)?;
                    let assembled = self.assembler.push(
                        frame.message_id,
                        frame.chunk_index,
                        frame.chunk_count,
                        chunk,
                        Instant::now(),
                    )?;
                    if let Some(message) = assembled {
                        return Ok(Some(RawFrame::from_payload(&message)?));
                    }
                }
                FrameKind::Ping => {
                    let pong = RelayFrame::pong(self.local_id, self.peer_id, frame.message_id);
                    self.framed.send(&pong).await?;
                }
                FrameKind::Pong => {}
                FrameKind::Close => return Ok(None),
                FrameKind::RouteRevoked => return Err(RemoteError::RouteRevoked),
                FrameKind::RelayError => {
                    return Err(frame
                        .relay_error_code()
                        .map_or(RemoteError::HandshakeClosed, RemoteError::Relay));
                }
                other @ (FrameKind::Auth | FrameKind::NoiseHandshake) => {
                    return Err(RemoteError::UnexpectedFrame(other));
                }
            }
        }
    }

    /// This endpoint's authenticated id.
    #[must_use]
    pub fn local_id(&self) -> Uuid {
        self.local_id
    }

    /// The pinned peer's id.
    #[must_use]
    pub fn peer_id(&self) -> Uuid {
        self.peer_id
    }

    /// The pinned peer's verified static public key.
    #[must_use]
    pub fn peer_static_key(&self) -> [u8; KEY_LEN] {
        self.peer_static
    }

    /// The relay wire protocol version this session speaks.
    #[must_use]
    pub fn protocol_version(&self) -> u16 {
        PROTOCOL_VERSION
    }

    /// Largest engine-frame payload this session can carry in one message,
    /// bounded by the relay body limit.
    #[must_use]
    pub fn max_engine_frame_bytes(&self) -> usize {
        MAX_BODY_LEN
    }
}

/// Drives an `EngineClient` on the **initiator** side: each `send` chunks/seals
/// one engine frame, each `recv` reassembles one. `sqlly-engine-protocol`'s
/// `EngineClient` can wrap a `PeerSession` directly.
#[async_trait]
impl<S> FramedTransport for PeerSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn recv(&mut self) -> std::result::Result<Option<RawFrame>, ProtocolError> {
        self.next_engine_frame()
            .await
            .map_err(RemoteError::into_protocol)
    }

    async fn send(&mut self, frame: &RawFrame) -> std::result::Result<(), ProtocolError> {
        self.send_engine_frame(frame)
            .await
            .map_err(RemoteError::into_protocol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default must sit under the hard cap, and the hard cap must equal the
    /// Noise ceiling minus tag, inner header, and chunk_count prefix.
    #[test]
    fn chunk_constants_respect_noise_ceiling() {
        assert_eq!(
            MAX_CHUNK_PLAINTEXT_LIMIT,
            65_535 - 16 - crate::noise::INNER_HEADER_LEN - 4
        );
        const { assert!(DEFAULT_MAX_CHUNK_PLAINTEXT <= MAX_CHUNK_PLAINTEXT_LIMIT) };
    }

    /// A config above the hard cap is clamped at use — no SessionConfig can
    /// make sealing exceed the Noise transport-message limit.
    #[test]
    fn oversized_chunk_config_is_clamped() {
        let cfg = SessionConfig {
            max_chunk_plaintext: 256 * 1024,
            ..SessionConfig::default()
        };
        assert_eq!(
            cfg.effective_max_chunk_plaintext(),
            MAX_CHUNK_PLAINTEXT_LIMIT
        );

        let cfg = SessionConfig::default();
        assert_eq!(
            cfg.effective_max_chunk_plaintext(),
            DEFAULT_MAX_CHUNK_PLAINTEXT
        );

        let cfg = SessionConfig {
            max_chunk_plaintext: MAX_CHUNK_PLAINTEXT_LIMIT,
            ..SessionConfig::default()
        };
        assert_eq!(
            cfg.effective_max_chunk_plaintext(),
            MAX_CHUNK_PLAINTEXT_LIMIT
        );
    }
}
