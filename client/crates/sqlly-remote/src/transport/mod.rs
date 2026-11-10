//! `sqlly-remote`: the shared client-side session/transport for the SQLLY
//! remote-engine path. Role-agnostic (initiator = controlling GPUI/CLI client,
//! responder = on-prem bridge): connect to a relay over TLS, speak `sqlly-wire`
//! frames, authenticate, run the `sqlly-noise` XX handshake with the peer
//! through the relay, chunk/encrypt engine frames, and expose an
//! `EngineClient`-compatible transport plus reconnect/backoff.
//!
//! Consumed by the bridge (responder) and, in Phase 3, the GPUI app
//! (initiator). The relay itself never links this crate — it only forwards
//! opaque `sqlly-wire` frames.
//!
//! # Layers
//!
//! * [`FramedStream`] — reads/writes [`RelayFrame`](sqlly_wire::RelayFrame)s
//!   over any `AsyncRead + AsyncWrite`, buffering incremental reads.
//! * [`authenticate`] — the opaque-token client→relay auth step.
//! * [`PeerSession`] — the Noise XX handshake through the relay (as
//!   [`Role::Initiator`] or [`Role::Responder`]), peer-key pinning, and the
//!   sealed, chunked engine-frame transport. It implements
//!   [`FramedTransport`](crate::engine_protocol::FramedTransport) so an
//!   `EngineClient` can drive the initiator side, and exposes
//!   [`send_engine_frame`](PeerSession::send_engine_frame) /
//!   [`next_engine_frame`](PeerSession::next_engine_frame) as the responder
//!   pump.
//! * [`connect_tls`] — the concrete production TLS byte stream, kept separable
//!   so tests use `tokio::io::duplex`.
//! * [`BackoffPolicy`] / [`backoff_delay`] — deterministic reconnect backoff.
//!
//! # Zero-knowledge
//!
//! Each engine frame is split into chunks, and every chunk is sealed
//! independently with Noise carrying authenticated inner metadata that binds the
//! `source`, `destination`, `message_id`, `chunk_index`, `flags`, **and**
//! `chunk_count`. The receiver checks that authenticated copy against the clear
//! outer relay header, so a relay cannot rewrite routing or chunk metadata
//! undetected.

#![forbid(unsafe_code)]

mod auth;
mod backoff;
mod error;
mod framed;
mod sealing;
mod session;
mod tls;

pub use auth::{auth_ack, authenticate};
pub use backoff::{BackoffPolicy, backoff_delay};
pub use error::{RemoteError, Result};
pub use framed::FramedStream;
pub use session::{
    AuthorizedPeer, DEFAULT_MAX_CHUNK_PLAINTEXT, MAX_CHUNK_PLAINTEXT_LIMIT, PeerSession, Role,
    SessionConfig,
};
pub use tls::{TlsTarget, connect_tls, connect_tls_with};

/// The rustls client config type [`connect_tls_with`] accepts, re-exported so
/// callers (e.g. the bridge's test harness) can name it without depending on
/// `tokio-rustls` directly.
pub use tokio_rustls::rustls::ClientConfig as TlsClientConfig;

/// The concrete encrypted byte stream [`connect_tls`] yields, re-exported so
/// callers can name the session type it carries without a `tokio-rustls`
/// dependency of their own.
pub type TlsByteStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;
