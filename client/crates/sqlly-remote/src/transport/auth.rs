//! The client-to-relay authentication step.
//!
//! The client sends an [`Auth`](sqlly_wire::FrameKind::Auth) frame whose body is
//! an **opaque** bearer token (issuance is API-side; this crate never inspects,
//! parses, or logs it) and whose `source` is the client's own authenticated id.
//! The relay replies with an `Auth` frame to acknowledge, or a
//! [`RelayError`](sqlly_wire::FrameKind::RelayError) to reject.
//!
//! The token is treated as a secret: it is never written to logs.

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};

use sqlly_wire::{FrameKind, RelayErrorCode, RelayFrame};
use uuid::Uuid;

use crate::transport::error::{RemoteError, Result};
use crate::transport::framed::FramedStream;

/// Message id used for the auth exchange (auth is not a chunked message).
const AUTH_MESSAGE_ID: u64 = 0;

/// Send an [`Auth`](FrameKind::Auth) frame carrying `token` and await the
/// relay's decision.
///
/// * `client_id` — this client's authenticated id (the frame `source`).
/// * `relay_id` — the relay's id (the frame `destination`).
/// * `token` — opaque bearer token bytes.
///
/// Returns `Ok(())` on an `Auth` acknowledgement, [`RemoteError::AuthFailed`] on
/// an `AuthFailed` relay error, and [`RemoteError::Relay`] for any other relay
/// error code.
pub async fn authenticate<S>(
    framed: &mut FramedStream<S>,
    client_id: Uuid,
    relay_id: Uuid,
    token: &[u8],
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let request = RelayFrame::new(
        FrameKind::Auth,
        client_id,
        relay_id,
        AUTH_MESSAGE_ID,
        Bytes::copy_from_slice(token),
    );
    framed.send(&request).await?;

    match framed.recv().await? {
        None => Err(RemoteError::HandshakeClosed),
        Some(resp) => match resp.kind {
            FrameKind::Auth => Ok(()),
            FrameKind::RelayError => match resp.relay_error_code() {
                Some(RelayErrorCode::AuthFailed) => Err(RemoteError::AuthFailed),
                Some(code) => Err(RemoteError::Relay(code)),
                None => Err(RemoteError::AuthFailed),
            },
            other => Err(RemoteError::UnexpectedFrame(other)),
        },
    }
}

/// Build the [`Auth`](FrameKind::Auth) acknowledgement a relay sends back to a
/// client that authenticated successfully. Provided so the relay/bridge and
/// tests speak the same shape. The body is empty.
#[must_use]
pub fn auth_ack(relay_id: Uuid, client_id: Uuid) -> RelayFrame {
    RelayFrame::new(
        FrameKind::Auth,
        relay_id,
        client_id,
        AUTH_MESSAGE_ID,
        Bytes::new(),
    )
}
