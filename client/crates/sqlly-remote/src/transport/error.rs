//! The crate-wide error type.
//!
//! No variant carries a payload body or secret material, so every error is safe
//! to log in the relay/bridge (the redaction discipline the threat model
//! requires). Ciphertext, plaintext, and Noise keys never appear in `Display`.

use crate::engine_protocol::ProtocolError;
use crate::noise::NoiseError;
use sqlly_wire::{AssembleError, ChunkError, RelayErrorCode, WireError};

/// Errors surfaced by the remote session/transport layer.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// Relay outer-frame encode/decode/validation failure.
    #[error("relay frame error: {0}")]
    Wire(#[from] WireError),

    /// Chunk splitting failure.
    #[error("chunk error: {0}")]
    Chunk(#[from] ChunkError),

    /// Chunk reassembly failure.
    #[error("reassembly error: {0}")]
    Assemble(#[from] AssembleError),

    /// Noise handshake/transport failure (decrypt failure, pin mismatch,
    /// metadata mismatch). Carries no key or payload material.
    #[error("noise error: {0}")]
    Noise(#[from] NoiseError),

    /// Engine protocol codec failure (e.g. a reassembled frame was malformed).
    #[error("engine protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// Underlying byte-stream I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The stream reached EOF in the middle of the relay auth or Noise
    /// handshake, before the exchange completed.
    #[error("stream closed during handshake")]
    HandshakeClosed,

    /// The relay rejected authentication.
    #[error("relay authentication failed")]
    AuthFailed,

    /// The relay reported an error frame (a non-authoritative transport hint).
    #[error("relay reported error: {0:?}")]
    Relay(RelayErrorCode),

    /// A previously granted route was revoked by the relay.
    #[error("relay route was revoked")]
    RouteRevoked,

    /// A frame of an unexpected kind arrived for the current protocol phase.
    #[error("unexpected relay frame kind: {0:?}")]
    UnexpectedFrame(sqlly_wire::FrameKind),

    /// A frame's authenticated source/destination did not match the pinned
    /// peer of this session (a spoofed or misrouted frame).
    #[error("frame source/destination did not match the session peer")]
    PeerMismatch,

    /// A decrypted chunk was too short to carry its bound `chunk_count` prefix.
    #[error("decrypted chunk is malformed")]
    MalformedChunk,

    /// The `chunk_count` bound inside the authenticated payload disagreed with
    /// the relay's clear outer header — a relay tried to rewrite it. This closes
    /// the metadata-integrity gap the outer-only chunk fields would otherwise
    /// leave open.
    #[error("chunk_count mismatch: outer header {outer}, authenticated inner {inner}")]
    ChunkCountMismatch {
        /// The `chunk_count` from the clear outer relay header.
        outer: u32,
        /// The `chunk_count` bound inside the authenticated payload.
        inner: u32,
    },
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, RemoteError>;

impl RemoteError {
    /// Map into a [`ProtocolError`] for the [`FramedTransport`] impl, preserving
    /// I/O as I/O and everything else as an opaque, payload-free decode message.
    ///
    /// For [`RemoteError::Relay`] the decode message is the variant's `Display`
    /// (`relay reported error: {code:?}`), which [`Self::relay_code_in`] parses
    /// back out — that round-trip is what lets a mid-session relay error stay
    /// typed across the `FramedTransport` boundary, so the message format is
    /// load-bearing (covered by `relay_code_survives_into_protocol`).
    ///
    /// [`FramedTransport`]: crate::engine_protocol::FramedTransport
    pub(crate) fn into_protocol(self) -> ProtocolError {
        match self {
            RemoteError::Io(e) => ProtocolError::Io(e),
            RemoteError::Protocol(e) => e,
            other => ProtocolError::decode(other.to_string()),
        }
    }

    /// Recover the [`RelayErrorCode`] embedded in a flattened transport
    /// message (the `Display` of [`RemoteError::Relay`], possibly wrapped in
    /// further context by downstream layers). `None` when the message does not
    /// carry one.
    #[must_use]
    pub fn relay_code_in(message: &str) -> Option<RelayErrorCode> {
        const MARKER: &str = "relay reported error: ";
        let start = message.find(MARKER)? + MARKER.len();
        let code = message[start..]
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()?;
        Some(match code {
            "DestOffline" => RelayErrorCode::DestOffline,
            "RoutingDenied" => RelayErrorCode::RoutingDenied,
            "AuthFailed" => RelayErrorCode::AuthFailed,
            "ProtocolError" => RelayErrorCode::ProtocolError,
            "RateLimited" => RelayErrorCode::RateLimited,
            "RelayFull" => RelayErrorCode::RelayFull,
            "SessionLimit" => RelayErrorCode::SessionLimit,
            _ => return None,
        })
    }

    /// The single source of user-facing copy for relay error codes, shared by
    /// session establishment (the GPUI `remote_session` mapping) and the
    /// mid-session transport boundary (`sqlly-remote-services`), so both paths
    /// surface identical guidance.
    pub fn relay_user_message(code: RelayErrorCode) -> &'static str {
        match code {
            RelayErrorCode::DestOffline => {
                "The relay device for this connection is offline. Start the SQLLY bridge on that \
                 machine and try again."
            }
            RelayErrorCode::RoutingDenied => {
                "This connection is not authorized to use that relay device. Re-authorize the \
                 pairing in your account."
            }
            RelayErrorCode::AuthFailed => {
                "Relay authentication failed. Sign in again to refresh your relay access."
            }
            RelayErrorCode::RelayFull => {
                "The relay server is at capacity. Please try again shortly."
            }
            RelayErrorCode::SessionLimit => {
                "The concurrent connection limit for this bridge has been reached. Free a slot \
                 from the relays page (or ask the bridge owner to), then try again."
            }
            RelayErrorCode::RateLimited => {
                "Too many relay requests. Please wait a moment and try again."
            }
            RelayErrorCode::ProtocolError => {
                "The relay rejected the session (protocol error). Please try again."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_CODES: [RelayErrorCode; 7] = [
        RelayErrorCode::DestOffline,
        RelayErrorCode::RoutingDenied,
        RelayErrorCode::AuthFailed,
        RelayErrorCode::ProtocolError,
        RelayErrorCode::RateLimited,
        RelayErrorCode::RelayFull,
        RelayErrorCode::SessionLimit,
    ];

    /// Every relay code must survive the flatten-to-`ProtocolError` boundary:
    /// the decode message `into_protocol` produces parses back to the same
    /// code, even after a downstream layer wraps it in more context.
    #[test]
    fn relay_code_survives_into_protocol() {
        for code in ALL_CODES {
            let flattened = RemoteError::Relay(code).into_protocol().to_string();
            assert_eq!(
                RemoteError::relay_code_in(&flattened),
                Some(code),
                "flattened: {flattened}"
            );
            let wrapped = format!("engine transport: {flattened} (mid-session)");
            assert_eq!(RemoteError::relay_code_in(&wrapped), Some(code));
        }
    }

    #[test]
    fn relay_code_in_rejects_unrelated_messages() {
        assert_eq!(RemoteError::relay_code_in("io error: reset"), None);
        assert_eq!(
            RemoteError::relay_code_in("relay reported error: SomethingNew"),
            None
        );
        assert_eq!(RemoteError::relay_code_in(""), None);
    }

    /// The copy is what both the establishment path and the mid-session path
    /// show verbatim; every code must have distinct, actionable text.
    #[test]
    fn relay_user_messages_are_distinct_and_actionable() {
        let mut seen = std::collections::HashSet::new();
        for code in ALL_CODES {
            let copy = RemoteError::relay_user_message(code);
            assert!(!copy.is_empty());
            assert!(seen.insert(copy), "duplicate copy for {code:?}");
        }
        assert!(
            RemoteError::relay_user_message(RelayErrorCode::DestOffline)
                .to_lowercase()
                .contains("offline")
        );
        assert!(
            RemoteError::relay_user_message(RelayErrorCode::RoutingDenied)
                .to_lowercase()
                .contains("not authorized")
        );
    }
}
