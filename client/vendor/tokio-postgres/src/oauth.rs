//! SQLLY patch: SASL `OAUTHBEARER` (RFC 7628) for PostgreSQL 18's `oauth`
//! authentication method.
//!
//! The client sends a GS2 header with no channel binding and no authzid
//! (`n,,`) followed by a single `auth` key/value pair carrying the bearer
//! token. A server that rejects the token (or receives an empty one, which
//! is how a client asks for discovery) answers with an
//! `AuthenticationSASLContinue` holding a JSON error document (`status`,
//! `scope`, `openid-configuration`); the client acknowledges with a lone
//! `0x01` and the server then ends the exchange with an `ErrorResponse`.
//! See <https://www.postgresql.org/docs/18/sasl-authentication.html#SASL-OAUTHBEARER>.

use std::error::Error;
use std::fmt;

/// The SASL mechanism name PostgreSQL advertises for `oauth` HBA entries.
pub const OAUTHBEARER: &str = "OAUTHBEARER";

/// The key/value separator (`kvsep`) of RFC 7628.
const KVSEP: u8 = 0x01;

/// Builds the client's initial response for `token`. An empty token yields
/// `n,,^Aauth=^A^A`, the discovery request.
pub fn client_initial_response(token: &str) -> Vec<u8> {
    let mut message = b"n,,".to_vec();
    message.push(KVSEP);
    message.extend_from_slice(b"auth=");
    if !token.is_empty() {
        message.extend_from_slice(b"Bearer ");
        message.extend_from_slice(token.as_bytes());
    }
    message.push(KVSEP);
    message.push(KVSEP);
    message
}

/// The client's reply to a server error document: a lone `kvsep`, which
/// tells the server to finish the failed exchange.
pub fn client_error_acknowledgement() -> Vec<u8> {
    vec![KVSEP]
}

/// Why an `OAUTHBEARER` sign-in did not complete. Carried as the source of an
/// authentication [`crate::Error`]; downcast it with
/// `error.source().and_then(|e| e.downcast_ref::<OAuthBearerError>())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthBearerError {
    /// A bearer token was configured but the server asked for something
    /// else: a password, or SASL without `OAUTHBEARER` among `mechanisms`.
    NotOffered {
        /// The SASL mechanisms the server listed (empty for non-SASL methods).
        mechanisms: Vec<String>,
    },
    /// The server refused the token (or answered a discovery request).
    Rejected {
        /// The server's JSON error document, verbatim.
        server_response: String,
        /// The message of the server's closing `ErrorResponse`, if it sent one.
        server_message: Option<String>,
    },
}

impl fmt::Display for OAuthBearerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OAuthBearerError::NotOffered { mechanisms } if mechanisms.is_empty() => {
                write!(f, "server did not offer OAUTHBEARER")
            }
            OAuthBearerError::NotOffered { mechanisms } => write!(
                f,
                "server did not offer OAUTHBEARER (offered: {})",
                mechanisms.join(", ")
            ),
            OAuthBearerError::Rejected {
                server_message: Some(message),
                ..
            } => write!(f, "server rejected the OAuth bearer token: {message}"),
            OAuthBearerError::Rejected { .. } => {
                write!(f, "server rejected the OAuth bearer token")
            }
        }
    }
}

impl Error for OAuthBearerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_response_frames_the_bearer_token() {
        assert_eq!(
            client_initial_response("abc.def"),
            b"n,,\x01auth=Bearer abc.def\x01\x01".to_vec()
        );
    }

    #[test]
    fn empty_token_is_the_discovery_request() {
        assert_eq!(
            client_initial_response(""),
            b"n,,\x01auth=\x01\x01".to_vec()
        );
    }
}
