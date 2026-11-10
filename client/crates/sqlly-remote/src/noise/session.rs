//! Noise XX handshake wrappers over `snow`, plus peer pinning.
//!
//! XX exchanges three messages:
//! 1. initiator -> responder:  `e`
//! 2. responder -> initiator:  `e, ee, s, es`
//! 3. initiator -> responder:  `s, se`
//!
//! After message 3 both sides hold a full-duplex transport and each has
//! learned the other's static public key, which the caller pins.

use crate::noise::identity::StaticKeypair;
use crate::noise::transport::Transport;
use crate::noise::{KEY_LEN, NOISE_PARAMS, NoiseError, Result};

/// Generous upper bound on Noise handshake framing overhead (two DH ephemeral
/// keys, a static key, and their AEAD tags), added on top of any payload.
const HANDSHAKE_OVERHEAD: usize = 256;

/// A driver for one side of a Noise XX handshake.
///
/// The caller pumps [`Self::write_message`] / [`Self::read_message`] in the
/// pattern's order (relaying the opaque bytes through the relay), then calls
/// [`Self::into_transport_pinned`] to verify the peer and obtain the
/// [`Transport`].
///
/// Not `Debug`-derived: it wraps `snow` handshake state that mixes in the local
/// static key.
pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    /// Build the initiator side using our device static key.
    pub fn initiator(local: &StaticKeypair) -> Result<Self> {
        Self::build(local, true)
    }

    /// Build the responder side using our device static key.
    pub fn responder(local: &StaticKeypair) -> Result<Self> {
        Self::build(local, false)
    }

    fn build(local: &StaticKeypair, initiator: bool) -> Result<Self> {
        let params = NOISE_PARAMS.parse().map_err(NoiseError::from)?;
        // snow 0.10 made `local_private_key` fallible (it validates the key
        // length up front) — propagate the error instead of chaining on `Self`.
        let builder = snow::Builder::new(params).local_private_key(local.private_bytes())?;
        let state = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        Ok(Self { state })
    }

    /// Produce the next handshake message to send to the peer, carrying an
    /// optional `payload` (usually empty for XX).
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; payload.len() + HANDSHAKE_OVERHEAD];
        let n = self.state.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume a handshake message received from the peer, returning any
    /// embedded payload.
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        // Decrypted payload can never exceed the message length; a small
        // cushion keeps snow happy on empty payloads.
        let mut buf = vec![0u8; message.len() + HANDSHAKE_OVERHEAD];
        let n = self.state.read_message(message, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Whether the three-message exchange has completed on this side.
    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// The peer's static public key, once learned (available after the message
    /// that transmits it: for the initiator after reading message 2, for the
    /// responder after reading message 3).
    pub fn remote_static(&self) -> Option<[u8; KEY_LEN]> {
        self.state.get_remote_static().and_then(|s| {
            let mut out = [0u8; KEY_LEN];
            if s.len() == KEY_LEN {
                out.copy_from_slice(s);
                Some(out)
            } else {
                None
            }
        })
    }

    /// Finish the handshake, verify the peer's static key against the pinned
    /// `expected_peer` key, and return the transport cipher on success.
    ///
    /// Rejects with [`NoiseError::HandshakeIncomplete`] if called early,
    /// [`NoiseError::NoRemoteStatic`] if no peer key was learned, and
    /// [`NoiseError::PeerPinMismatch`] on a pin mismatch.
    pub fn into_transport_pinned(self, expected_peer: &[u8; KEY_LEN]) -> Result<Transport> {
        if !self.state.is_handshake_finished() {
            return Err(NoiseError::HandshakeIncomplete);
        }
        let remote = self
            .state
            .get_remote_static()
            .ok_or(NoiseError::NoRemoteStatic)?;
        if remote.len() != KEY_LEN || remote != expected_peer.as_slice() {
            return Err(NoiseError::PeerPinMismatch);
        }
        let ts = self.state.into_transport_mode()?;
        Ok(Transport::new(ts))
    }

    /// Finish the handshake without pinning (caller pins separately via
    /// [`Self::remote_static`]). Prefer [`Self::into_transport_pinned`].
    pub fn into_transport_unchecked(self) -> Result<Transport> {
        if !self.state.is_handshake_finished() {
            return Err(NoiseError::HandshakeIncomplete);
        }
        let ts = self.state.into_transport_mode()?;
        Ok(Transport::new(ts))
    }
}
