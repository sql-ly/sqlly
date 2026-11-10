//! `sqlly-noise`: device static-key identity (keyring-backed), Noise XX
//! handshakes, peer pinning, and per-chunk transport encryption for the SQLLY
//! remote-engine transport. Pattern: `Noise_XX_25519_ChaChaPoly_BLAKE2s`.
//!
//! This crate never sees relay routing; it takes plaintext chunks (already
//! carrying authenticated inner metadata) and produces ciphertext for
//! `sqlly-wire` frame bodies, and the reverse. It has **no** dependency on
//! `sqlly-wire`: it operates on plain fields and byte slices so it can be
//! reused for any future zero-knowledge peer traffic.
//!
//! ## Security invariants
//! - The 32-byte static **private** key never appears in `Debug` output or in
//!   any error `Display`/`Debug`. See [`identity::StaticKeypair`].
//! - Test code must never touch the real OS keyring; the secret store is
//!   abstracted behind [`identity::SecretStore`] with an in-memory mock
//!   ([`identity::InMemorySecretStore`]).
//! - After a handshake the peer's static public key is verified against a
//!   pinned expected value; a mismatch is rejected with
//!   [`NoiseError::PeerPinMismatch`].
//! - Each chunk is sealed independently and carries authenticated inner
//!   metadata that the receiver compares against the relay's clear outer frame
//!   header, defeating relay-side metadata rewriting.

// `deny` rather than `forbid`: identity.rs needs one scoped
// `#[allow(unsafe_code)]` for the best-effort key zeroize on drop.
#![deny(unsafe_code)]

mod hex;

pub mod identity;
pub mod session;
pub mod transport;

pub use identity::{
    FileSecretStore, InMemorySecretStore, OsKeyringStore, SecretStore, StaticKeypair,
};
pub use session::Handshake;
pub use transport::{INNER_HEADER_LEN, InnerChunk, OuterMeta, Transport};

/// The single Noise handshake pattern this crate speaks.
pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Keyring service name for persisted Noise static keys. Stable across
/// releases so an existing device keeps its identity. The Mac App Store
/// edition uses its own name so it never reads the direct download's item
/// (which makes macOS ask for the login password).
#[cfg(not(feature = "mac-app-store"))]
pub const KEYRING_SERVICE: &str = "SQLLY.relay-noise";
#[cfg(feature = "mac-app-store")]
pub const KEYRING_SERVICE: &str = "SQLLY.relay-noise.mac-app-store";

/// Length in bytes of an X25519 static key (private or public).
pub const KEY_LEN: usize = 32;

/// Errors produced by this crate.
///
/// No variant carries secret key material: `Display`/`Debug` on any error is
/// safe to log. In particular the `snow` error is surfaced by its own opaque
/// message, which never includes key bytes.
#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    /// Underlying Noise/`snow` protocol failure (bad message, decrypt failure,
    /// wrong state, etc.). Carries no key material.
    #[error("noise protocol error: {0}")]
    Snow(String),

    /// OS keyring access failure (not the benign "no entry" case, which is
    /// modeled as `Ok(None)` at the store layer).
    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),

    /// A pluggable secret store reported a failure.
    #[error("secret store error: {0}")]
    Store(String),

    /// A persisted key was not exactly [`KEY_LEN`] bytes.
    #[error("stored static key has invalid length")]
    InvalidKeyLength,

    /// Persisted key material was not valid hex.
    #[error("stored static key is not valid hex")]
    InvalidKeyEncoding,

    /// A transport/pin operation was attempted before the handshake finished.
    #[error("noise handshake is not finished")]
    HandshakeIncomplete,

    /// The handshake completed but no remote static key was learned (should not
    /// happen for XX; guards against misuse).
    #[error("no remote static key available after handshake")]
    NoRemoteStatic,

    /// The peer's static public key did not match the pinned expected key.
    #[error("peer static key does not match pinned key")]
    PeerPinMismatch,

    /// A decrypted chunk was too short to contain the inner metadata header.
    #[error("decrypted chunk is too short for inner metadata")]
    ChunkTooShort,

    /// The authenticated inner metadata disagreed with the clear outer frame
    /// header — a relay tried to rewrite routing/chunk metadata.
    #[error("inner chunk metadata does not match outer frame header")]
    MetadataMismatch,
}

impl From<snow::Error> for NoiseError {
    fn from(e: snow::Error) -> Self {
        // Render via `Display` so we never risk embedding structured key
        // material; `snow::Error` messages are opaque and key-free.
        NoiseError::Snow(e.to_string())
    }
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, NoiseError>;
