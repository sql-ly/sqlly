//! Relay-proxy transport — the browser's way to reach real databases.
//!
//! A relay started with `--proxy-credentials` serves the servers and
//! databases of a credential file over a WebSocket (`sqlly-relay`'s
//! `proxy` module). This module is the browser end of that contract:
//! [`connect`] opens the socket, reads the proxy's `hello` (the secret-free
//! connection catalog), and wraps the socket as a
//! [`FramedTransport`] so the SAME remote-engine ports the desktop app uses
//! for Via-Relay connections ([`crate::remote::RemoteServices`]) drive every
//! feature — queries, catalog sync, metadata, row editing — through the
//! proxy's engine.
//!
//! Every JavaScript value is `!Send`, but the service ports need `Send`
//! futures. The `WebSocket` therefore lives in a thread-local registry keyed
//! by session id (the browser has exactly one thread) and the transport
//! handle carries only that id plus a `Send` channel the socket's callbacks
//! feed, mirroring the PGlite bridge in [`super::pglite`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use sqlly_core::StudioError;
use sqlly_core::credential_proxy::{
    LaunchOpen, PROXY_PROTOCOL_MIN_SUPPORTED, PROXY_PROTOCOL_VERSION, ProxyCatalog, ProxyLicense,
    ProxyServerMessage,
};
use sqlly_remote::engine_protocol::{FramedTransport, ProtocolError, RawFrame};
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use crate::Services;
use crate::remote::RemoteServices;

/// Why [`connect`] did not produce a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The proxy answered and said no: the launch token is unknown or
    /// expired, the license is not ok, the seat has no session left. `code`
    /// is the proxy's close code (`4000 + <HTTP status>`, so `4401`, `4429`,
    /// `4503`). Retrying changes nothing; the page has to be relaunched.
    Refused { code: u16, message: String },
    /// The socket could not be opened or died before the hello: the proxy is
    /// down, unreachable, or refused the upgrade outright. Worth retrying.
    Unavailable(String),
}

impl ConnectError {
    pub fn display_message(&self) -> sqlly_core::DisplayMessage {
        sqlly_core::DisplayMessage::raw(self.message())
    }

    /// The sentence to show the user.
    pub fn message(&self) -> &str {
        match self {
            Self::Refused { message, .. } | Self::Unavailable(message) => message,
        }
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl From<ConnectError> for StudioError {
    fn from(error: ConnectError) -> Self {
        StudioError::Connection(error.message().to_string())
    }
}

/// Whether a WebSocket close code is one of the proxy's refusal codes.
fn is_refusal_code(code: u16) -> bool {
    (4000..5000).contains(&code)
}

/// An open proxy session: what the proxy said it serves, and the services
/// that run through it.
pub struct ProxySession {
    /// The proxy's label from its hello (the relay's advertised hostname).
    pub relay: String,
    /// The secret-free connection listing.
    pub catalog: ProxyCatalog,
    /// Remote-backed services over the proxy socket; install these for every
    /// proxied profile.
    pub services: Services,
    /// Flips to `true` when the socket closes for any reason.
    pub closed: tokio::sync::watch::Receiver<bool>,
    /// Why the socket closed, once it has: the proxy's last `error` message
    /// (the engine stream broke, the administrator released the seat, …) or
    /// the close frame's reason. `None` while open or when it closed silently.
    pub close_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// The embedded license state: the hello's value, then every `license`
    /// push. `None` for a plain credential-file proxy (no license at all).
    pub license: tokio::sync::watch::Receiver<Option<ProxyLicense>>,
    /// Query tabs the host app asked this launch to open (SQLly Embedded
    /// launch context). Empty for a plain proxy and for every session after
    /// the first one a launch token opens.
    pub open: Vec<LaunchOpen>,
}

/// What the socket's JS callbacks push to the transport.
type Inbound = Result<RawFrame, ProtocolError>;

struct SocketEntry {
    socket: web_sys::WebSocket,
    /// Keeps the event callbacks alive for the socket's lifetime.
    _callbacks: Vec<Closure<dyn FnMut(JsValue)>>,
}

thread_local! {
    static SOCKETS: RefCell<HashMap<u64, SocketEntry>> = RefCell::new(HashMap::new());
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// [`FramedTransport`] over a browser WebSocket: one binary message per
/// engine frame payload (`[tag][body]`), no length prefix.
pub struct WsTransport {
    id: u64,
    inbound: mpsc::UnboundedReceiver<Inbound>,
}

#[async_trait]
impl FramedTransport for WsTransport {
    async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
        match self.inbound.recv().await {
            Some(Ok(frame)) => Ok(Some(frame)),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        }
    }

    async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
        let payload = frame.to_payload();
        let id = self.id;
        SOCKETS.with(|sockets| {
            let sockets = sockets.borrow();
            let entry = sockets
                .get(&id)
                .ok_or_else(|| ProtocolError::decode("proxy socket is closed"))?;
            entry
                .socket
                .send_with_u8_array(&payload)
                .map_err(|e| ProtocolError::decode(format!("proxy send failed: {}", js_text(&e))))
        })
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        close_socket(self.id);
    }
}

fn close_socket(id: u64) {
    SOCKETS.with(|sockets| {
        if let Some(entry) = sockets.borrow_mut().remove(&id) {
            let _ = entry.socket.close();
        }
    });
}

fn js_text(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"))
}

/// The `WebSocket` URL for a proxy `url` plus optional token — accepts the
/// `wss://host:port/path` form the operator hands out, or an `https://` /
/// `http://` one, which is rewritten to the socket scheme.
pub fn socket_url(url: &str, token: Option<&str>) -> String {
    let trimmed = url.trim();
    let mut out = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        trimmed.to_string()
    };
    if let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) {
        let (base, fragment) = match out.split_once('#') {
            Some((b, f)) => (b.to_string(), Some(f.to_string())),
            None => (out.clone(), None),
        };
        let has_token = base
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|p| p.starts_with("token=")));
        out = if has_token {
            base
        } else if base.contains('?') {
            format!("{base}&token={token}")
        } else {
            format!("{base}?token={token}")
        };
        if let Some(f) = fragment {
            out.push('#');
            out.push_str(&f);
        }
    }
    out
}

/// Open a session to the relay proxy at `url` (see [`socket_url`]).
///
/// Resolves once the proxy's hello has arrived, so the caller can install
/// the listing and the services together; the socket stays open for the
/// life of the returned services.
pub async fn connect(url: &str) -> Result<ProxySession, ConnectError> {
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let (hello_tx, hello_rx) = oneshot::channel::<Result<ProxyServerMessage, ConnectError>>();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<Inbound>();
    let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
    let (license_tx, license_rx) = tokio::sync::watch::channel::<Option<ProxyLicense>>(None);
    let license_tx = std::rc::Rc::new(license_tx);

    let socket = web_sys::WebSocket::new(url).map_err(|e| {
        ConnectError::Unavailable(format!("open proxy socket {url}: {}", js_text(&e)))
    })?;
    socket.set_binary_type(web_sys::BinaryType::Arraybuffer);

    // Shared between the callbacks: the hello waiter (resolved by the hello,
    // or by the close that follows a refusal), the inbound sender (dropped on
    // close so the transport sees EOF), and the last reason the proxy gave.
    let hello_tx = std::rc::Rc::new(RefCell::new(Some(hello_tx)));
    let inbound_tx = std::rc::Rc::new(RefCell::new(Some(inbound_tx)));
    let close_reason: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    let on_message = {
        let inbound_tx = inbound_tx.clone();
        let license_tx = license_tx.clone();
        let hello_tx = hello_tx.clone();
        let close_reason = std::sync::Arc::clone(&close_reason);
        Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            let Ok(event) = event.dyn_into::<web_sys::MessageEvent>() else {
                return;
            };
            let data = event.data();
            if let Some(text) = data.as_string() {
                let message: Result<ProxyServerMessage, ConnectError> = serde_json::from_str(&text)
                    .map_err(|e| ConnectError::Unavailable(format!("bad proxy message: {e}")));
                match message {
                    Ok(ProxyServerMessage::Error { message }) => {
                        // Before the hello this is a refusal; the close frame
                        // that follows carries the code and resolves the
                        // waiter. After the hello it is why the session is
                        // about to end.
                        *close_reason.lock().expect("close reason poisoned") =
                            Some(message.clone());
                        if let Some(tx) = inbound_tx.borrow().as_ref() {
                            let _ = tx.send(Err(ProtocolError::decode(format!(
                                "proxy error: {message}"
                            ))));
                        }
                    }
                    Ok(ProxyServerMessage::License {
                        state,
                        plan,
                        message,
                    }) => {
                        license_tx.send_replace(Some(ProxyLicense {
                            state,
                            plan,
                            message,
                        }));
                    }
                    other => {
                        if let Some(tx) = hello_tx.borrow_mut().take() {
                            let _ = tx.send(other);
                        }
                    }
                }
                return;
            }
            let Ok(buffer) = data.dyn_into::<js_sys::ArrayBuffer>() else {
                return;
            };
            let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
            if let Some(tx) = inbound_tx.borrow().as_ref() {
                let _ = tx.send(RawFrame::from_payload(&bytes));
            }
        })
    };
    let on_close = {
        let hello_tx = hello_tx.clone();
        let inbound_tx = inbound_tx.clone();
        let close_reason = std::sync::Arc::clone(&close_reason);
        Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            let (code, frame_reason) = event
                .dyn_ref::<web_sys::CloseEvent>()
                .map(|e| (e.code(), e.reason()))
                .unwrap_or((0, String::new()));
            let said = close_reason.lock().expect("close reason poisoned").clone();
            if said.is_none() && !frame_reason.is_empty() {
                *close_reason.lock().expect("close reason poisoned") = Some(frame_reason.clone());
            }
            if let Some(tx) = hello_tx.borrow_mut().take() {
                // Closed before the hello. A refusal announced itself (an
                // `error` message and/or a 4xxx code); anything else means the
                // proxy could not be reached at all — browsers hide the HTTP
                // status of a failed upgrade, so 1006 is all there is.
                let error = match said {
                    Some(message) => ConnectError::Refused { code, message },
                    None if is_refusal_code(code) => ConnectError::Refused {
                        code,
                        message: if frame_reason.is_empty() {
                            format!("the proxy refused this session (code {code})")
                        } else {
                            frame_reason
                        },
                    },
                    None => ConnectError::Unavailable(format!(
                        "could not reach the relay proxy (socket closed with code {code}); \
                         check the URL and that the page may connect to that origin"
                    )),
                };
                let _ = tx.send(Err(error));
            }
            // Dropping the sender ends the transport's stream cleanly.
            inbound_tx.borrow_mut().take();
            let _ = closed_tx.send(true);
        })
    };
    // The browser hides the real reason from scripts and always follows an
    // error with a `close` event, which carries the code: that handler does
    // the reporting, so nothing is decided here.
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |_event: JsValue| {});
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    SOCKETS.with(|sockets| {
        sockets.borrow_mut().insert(
            id,
            SocketEntry {
                socket,
                _callbacks: vec![on_message, on_close, on_error],
            },
        );
    });

    let hello = match hello_rx.await {
        Ok(Ok(hello)) => hello,
        Ok(Err(error)) => {
            close_socket(id);
            return Err(error);
        }
        Err(_) => {
            close_socket(id);
            return Err(ConnectError::Unavailable(
                "proxy session ended before it said hello".into(),
            ));
        }
    };
    let ProxyServerMessage::Hello {
        protocol,
        relay,
        catalog,
        license,
        open,
    } = hello
    else {
        close_socket(id);
        return Err(ConnectError::Unavailable(
            "proxy sent an unexpected first message".into(),
        ));
    };
    if !protocol_supported(protocol) {
        close_socket(id);
        return Err(ConnectError::Unavailable(format!(
            "proxy speaks protocol v{protocol}; this build speaks v{PROXY_PROTOCOL_VERSION} — update the older side"
        )));
    }
    // The hello's license is the first value; pushes replace it.
    license_tx.send_replace(license);

    let transport = WsTransport {
        id,
        inbound: inbound_rx,
    };
    let remote = RemoteServices::from_transport(transport);
    // Grid runs keep large results in browser storage; metadata and every
    // other caller see the remote executor as is.
    let mut services = remote.services().clone();
    services.queries = std::sync::Arc::new(super::spill::SpillingQueryExecutor::new(
        services.queries.clone(),
    ));
    Ok(ProxySession {
        relay,
        catalog,
        services,
        closed: closed_rx,
        close_reason,
        license: license_rx,
        open,
    })
}

/// Whether this build can talk to a proxy announcing `protocol`: the
/// framing has not changed since v1, so every version from the minimum up
/// to this build's own is accepted (a v1 proxy simply sends no license).
pub fn protocol_supported(protocol: u32) -> bool {
    (PROXY_PROTOCOL_MIN_SUPPORTED..=PROXY_PROTOCOL_VERSION).contains(&protocol)
}

#[cfg(test)]
mod tests {
    use super::{ConnectError, is_refusal_code, protocol_supported, socket_url};

    #[test]
    fn refusal_codes_are_the_application_range() {
        assert!(is_refusal_code(4401));
        assert!(is_refusal_code(4503));
        assert!(!is_refusal_code(1006));
        assert!(!is_refusal_code(1000));
        let refused = ConnectError::Refused {
            code: 4429,
            message: "alice already has the maximum number of SQLly sessions open".into(),
        };
        assert_eq!(refused.message(), refused.to_string());
    }

    #[test]
    fn protocol_one_and_two_are_accepted_three_is_not() {
        assert!(protocol_supported(1));
        assert!(protocol_supported(2));
        assert!(!protocol_supported(3));
        assert!(!protocol_supported(0));
    }

    #[test]
    fn socket_url_normalizes_scheme_and_appends_token_once() {
        assert_eq!(
            socket_url("https://relay.example:8444/proxy", Some("t")),
            "wss://relay.example:8444/proxy?token=t"
        );
        assert_eq!(
            socket_url("http://localhost:8444/proxy?x=1", Some("t")),
            "ws://localhost:8444/proxy?x=1&token=t"
        );
        assert_eq!(
            socket_url("wss://relay.example/proxy?token=given", Some("t")),
            "wss://relay.example/proxy?token=given"
        );
        assert_eq!(
            socket_url(" wss://relay.example/proxy ", None),
            "wss://relay.example/proxy"
        );
        assert_eq!(
            socket_url("wss://r/proxy#frag", Some("t")),
            "wss://r/proxy?token=t#frag"
        );
    }
}
