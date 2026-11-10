//! `$/cancelRequest` plumbing. The read loop records cancelled request ids in
//! [`Shared::cancelled`](super::Shared); the spawned request tasks consult (and
//! consume) those entries through these helpers so every request still gets
//! exactly one response, as the LSP spec requires.

use std::sync::{Arc, Mutex};

use crate::jsonrpc::{Id, Message, error_code};

use super::Shared;

/// `$/cancelRequest` params: the id of the request to cancel.
#[derive(Debug, Clone, serde::Deserialize)]
pub(super) struct CancelParams {
    pub(super) id: Id,
}

/// Canonical string key for a request id (ints and strings share one set).
pub(super) fn id_key(id: &Id) -> String {
    match id {
        Id::Int(n) => format!("i:{n}"),
        Id::Str(s) => format!("s:{s}"),
    }
}

/// Whether `id` was cancelled via `$/cancelRequest`; consumes the entry.
pub(super) fn take_cancelled(shared: &Arc<Mutex<Shared>>, id: Option<&Id>) -> bool {
    let Some(id) = id else { return false };
    shared.lock().unwrap().cancelled.remove(&id_key(id))
}

/// Build the response for a spawned request, honoring `$/cancelRequest`: a
/// cancelled request answers with `REQUEST_CANCELLED` (as the LSP spec
/// requires — every request gets exactly one response) instead of its result.
pub(super) fn respond_unless_cancelled(
    shared: &Arc<Mutex<Shared>>,
    id: Id,
    result: impl FnOnce() -> serde_json::Value,
) -> Message {
    if take_cancelled(shared, Some(&id)) {
        Message::error(
            id,
            error_code::REQUEST_CANCELLED,
            "request cancelled".to_string(),
        )
    } else {
        Message::response(id, result())
    }
}

/// Pre-work cancellation probe for spawned request handlers: `Some(reply)`
/// when the request was already cancelled (consuming the entry), letting the
/// task answer without doing the work at all.
pub(super) fn cancelled_reply(shared: &Arc<Mutex<Shared>>, id: &Id) -> Option<Message> {
    take_cancelled(shared, Some(id)).then(|| {
        Message::error(
            id.clone(),
            error_code::REQUEST_CANCELLED,
            "request cancelled".to_string(),
        )
    })
}
