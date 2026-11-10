//! The remote-engine transport: everything between a SQLLY client and an
//! engine it does not host itself.
//!
//! `noise` is the zero-knowledge handshake and key storage,
//! `engine_protocol` the request/response vocabulary, and `transport` the
//! client end that dials a bridge directly or rides a relay. The framing
//! itself stays in `sqlly-wire`, which the relay links *alone* — that crate
//! boundary is what proves the relay cannot decrypt what it forwards.

pub mod engine_protocol;
#[cfg(not(target_arch = "wasm32"))]
pub mod noise;
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;
