//! Loopback HTTP server that captures the OAuth redirect from the browser.
//!
//! Microsoft v2.0 authorize redirects the user's browser to
//! `http://127.0.0.1:<port>/auth?code=...&state=...` after sign-in. We bind
//! 127.0.0.1 to one of three fixed ports (chosen at random each sign-in),
//! accept one HTTP request, parse the query, return a small confirmation HTML
//! page, and shut down. Read-only with respect to the user's machine: the
//! listener only binds the loopback interface, never a public NIC, so the
//! redirect capture is invisible to other hosts on the network.
//!
//! The three ports are fixed and published so they can be pre-registered as
//! redirect URIs in the Entra app registration:
//!   - `http://127.0.0.1:8423/auth`
//!   - `http://127.0.0.1:8424/auth`
//!   - `http://127.0.0.1:8425/auth`
//!
//! Pure tokio + std — no gpui dep — so it sits in `sqlly-security` alongside
//! the rest of the auth flow logic. The orchestrator awaits
//! [`LoopbackServer::port`] to compose the authorize URL, then awaits
//! [`LoopbackServer::wait_for_callback`] for the captured result.

use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

/// The three fixed loopback ports the OAuth redirect listener cycles through.
/// Register all three as redirect URIs in your Entra app registration:
/// `http://127.0.0.1:{port}/auth` for each.
pub const LOOPBACK_PORTS: [u16; 3] = [8423, 8424, 8425];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopbackCallback {
    Code {
        code: String,
        state: Option<String>,
    },
    Error {
        error: String,
        description: Option<String>,
    },
}

#[derive(Debug, Error)]
pub enum LoopbackError {
    #[error("could not bind loopback redirect listener: {0}")]
    Bind(String),
    #[error("all fixed loopback redirect ports are in use: {0:?}")]
    AllPortsInUse(Vec<u16>),
    #[error("timed out waiting for the OAuth redirect after {0:?}")]
    Timeout(Duration),
    /// Multiple callbacks landed on the listener. Cannot happen as written —
    /// the accept loop stops after the first request — so this is purely
    /// defensive against a future refactor that leaks the listener task.
    #[error("multiple redirect callbacks landed on the listener")]
    Multiple,
    /// A single query string was provided that the local capture server could
    /// not parse. Should not happen in normal use; surfaces so the orchestrator
    /// can show "Browser sent an unexpected response" instead of hanging.
    #[error("malformed redirect query: {0}")]
    Malformed(String),
}

impl LoopbackError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::Bind(detail) => DisplayMessage::new(
                "services.security.loopback.errors.bind",
                format!("could not bind loopback redirect listener: {detail}"),
            )
            .with_arg("detail", detail),
            Self::AllPortsInUse(ports) => DisplayMessage::new(
                "services.security.loopback.errors.all_ports_in_use",
                format!("all fixed loopback redirect ports are in use: {ports:?}"),
            )
            .with_arg("ports", format!("{ports:?}")),
            Self::Timeout(duration) => DisplayMessage::new(
                "services.security.loopback.errors.timeout",
                format!("timed out waiting for the OAuth redirect after {duration:?}"),
            )
            .with_arg("duration", format!("{duration:?}")),
            Self::Multiple => DisplayMessage::new(
                "services.security.loopback.errors.multiple",
                "multiple redirect callbacks landed on the listener",
            ),
            Self::Malformed(detail) => DisplayMessage::new(
                "services.security.loopback.errors.malformed",
                format!("malformed redirect query: {detail}"),
            )
            .with_arg("detail", detail),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopbackPages {
    success: LoopbackPageCopy,
    failure: LoopbackPageCopy,
    bad_request: String,
    not_found: String,
}

#[derive(Debug, Clone)]
struct LoopbackPageCopy {
    title: String,
    heading: String,
    body: String,
    footer: String,
    accent: &'static str,
}

impl LoopbackPages {
    pub fn english() -> Self {
        Self::localized(|message| message.into_fallback())
    }

    /// Resolve every browser-page string on the UI thread before starting the
    /// listener. The resolver is never called by the background capture task.
    pub fn localized(mut resolve: impl FnMut(sqlly_core::DisplayMessage) -> String) -> Self {
        let page =
            |success: bool, resolve: &mut dyn FnMut(sqlly_core::DisplayMessage) -> String| {
                let mut part = |key: &'static str, fallback: &'static str| {
                    resolve(sqlly_core::DisplayMessage::new(key, fallback))
                };
                if success {
                    LoopbackPageCopy {
                        title: part(
                            "services.security.loopback.pages.success.title",
                            "SQLLY sign-in",
                        ),
                        heading: part(
                            "services.security.loopback.pages.success.heading",
                            "Sign-in complete",
                        ),
                        body: part(
                            "services.security.loopback.pages.success.body",
                            "You can close this browser tab and return to SQLLY. SQLLY has received your Microsoft Entra credentials and is finishing account setup.",
                        ),
                        footer: part(
                            "services.security.loopback.pages.success.footer",
                            "SQLLY — Microsoft Entra ID sign-in",
                        ),
                        accent: "#107C10",
                    }
                } else {
                    LoopbackPageCopy {
                        title: part(
                            "services.security.loopback.pages.failure.title",
                            "SQLLY sign-in",
                        ),
                        heading: part(
                            "services.security.loopback.pages.failure.heading",
                            "Sign-in failed",
                        ),
                        body: part(
                            "services.security.loopback.pages.failure.body",
                            "Microsoft Entra ID returned an error. Close this tab and try again from SQLLY.",
                        ),
                        footer: part(
                            "services.security.loopback.pages.failure.footer",
                            "SQLLY — Microsoft Entra ID sign-in",
                        ),
                        accent: "#A1260E",
                    }
                }
            };
        Self {
            success: page(true, &mut resolve),
            failure: page(false, &mut resolve),
            bad_request: resolve(sqlly_core::DisplayMessage::new(
                "services.security.loopback.pages.bad_request",
                "Bad request.",
            )),
            not_found: resolve(sqlly_core::DisplayMessage::new(
                "services.security.loopback.pages.not_found",
                "Not found.",
            )),
        }
    }
}

/// Single-shot loopback HTTP server bound to 127.0.0.1 on a free OS-assigned
/// port. The accept loop stops after processing one request — the OAuth
/// redirect only arrives once per sign-in attempt.
pub struct LoopbackServer {
    port: u16,
    /// Receiver end of the oneshot that delivers the captured callback to
    /// [`Self::wait_for_callback`]. Held inside the server struct (not in the
    /// accept task) so the wait future can `await` on it directly, without
    /// hand-rolling channel wiring across an `Arc`.
    callback_rx: Mutex<Option<oneshot::Receiver<Result<LoopbackCallback, LoopbackError>>>>,
}

impl LoopbackServer {
    /// Binds 127.0.0.1 to one of [`LOOPBACK_PORTS`] chosen at random, trying
    /// each in shuffled order until one succeeds. Returns the
    /// [`LoopbackServer`] whose `port` is immediately known so the
    /// orchestrator can compose the authorize URL while the listener task is
    /// starting up.
    pub async fn bind(
        expected_path: &'static str,
        expected_state: impl Into<String>,
        idle_timeout: Duration,
    ) -> Result<Arc<Self>, LoopbackError> {
        Self::bind_with_pages(
            expected_path,
            expected_state,
            idle_timeout,
            LoopbackPages::english(),
        )
        .await
    }

    pub async fn bind_with_pages(
        expected_path: &'static str,
        expected_state: impl Into<String>,
        idle_timeout: Duration,
        pages: LoopbackPages,
    ) -> Result<Arc<Self>, LoopbackError> {
        let mut ports: Vec<u16> = LOOPBACK_PORTS.to_vec();
        shuffle(&mut ports);
        let expected_state: String = expected_state.into();
        let mut last_err = String::new();
        for port in &ports {
            match Self::try_bind_to_port(
                *port,
                expected_path,
                expected_state.clone(),
                idle_timeout,
                pages.clone(),
            )
            .await
            {
                Ok(server) => return Ok(server),
                Err(LoopbackError::Bind(e)) => {
                    last_err = e;
                }
                Err(other) => return Err(other),
            }
        }
        let _ = last_err;
        Err(LoopbackError::AllPortsInUse(ports))
    }

    /// Binds to a specific port (used by tests; production uses [`Self::bind`]
    /// which cycles through [`LOOPBACK_PORTS`]). Pass `0` to get an
    /// OS-assigned ephemeral port.
    pub async fn bind_to_port(
        port: u16,
        expected_path: &'static str,
        expected_state: impl Into<String>,
        idle_timeout: Duration,
    ) -> Result<Arc<Self>, LoopbackError> {
        Self::try_bind_to_port(
            port,
            expected_path,
            expected_state,
            idle_timeout,
            LoopbackPages::english(),
        )
        .await
    }

    async fn try_bind_to_port(
        port: u16,
        expected_path: &'static str,
        expected_state: impl Into<String>,
        idle_timeout: Duration,
        pages: LoopbackPages,
    ) -> Result<Arc<Self>, LoopbackError> {
        let addr = format!("127.0.0.1:{port}");
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| LoopbackError::Bind(format!("{addr}: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| LoopbackError::Bind(e.to_string()))?
            .port();
        let expected_state: String = expected_state.into();
        let (callback_tx, callback_rx) = oneshot::channel();
        let server = Arc::new(Self {
            port,
            callback_rx: Mutex::new(Some(callback_rx)),
        });
        let server_for_task = Arc::clone(&server);
        tokio::spawn(async move {
            match tokio::time::timeout(
                idle_timeout,
                accept_loop(
                    listener,
                    &server_for_task,
                    expected_path,
                    &expected_state,
                    pages,
                ),
            )
            .await
            {
                Ok(Ok(callback)) => {
                    let _ = callback_tx.send(Ok(callback));
                }
                Ok(Err(error)) => {
                    let _ = callback_tx.send(Err(error));
                }
                Err(_elapsed) => {
                    let _ = callback_tx.send(Err(LoopbackError::Timeout(idle_timeout)));
                }
            }
        });
        Ok(server)
    }

    /// The OS-assigned port the listener was bound to. Read this immediately
    /// after [`Self::bind`] to compose the authorize URL before opening the
    /// browser — the listener task is starting up in parallel.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Awaits the captured callback or the bounded idle timeout. Returns
    /// `Ok(callback)` on a normal OAuth response, `Err(LoopbackError::Timeout)`
    /// if no request arrived before `idle_timeout` expired.
    pub async fn wait_for_callback(&self) -> Result<LoopbackCallback, LoopbackError> {
        let mut guard = self.callback_rx.lock().await;
        let rx = match guard.take() {
            Some(rx) => rx,
            None => return Err(LoopbackError::Multiple),
        };
        rx.await
            .map_err(|_| LoopbackError::Bind("listener task ended".into()))?
    }
}

async fn accept_loop(
    listener: TcpListener,
    _server: &LoopbackServer,
    expected_path: &str,
    _expected_state: &str,
    pages: LoopbackPages,
) -> Result<LoopbackCallback, LoopbackError> {
    let (stream, _) = listener
        .accept()
        .await
        .map_err(|e| LoopbackError::Bind(e.to_string()))?;
    handle_request(stream, expected_path, pages).await
}

async fn handle_request(
    mut stream: tokio::net::TcpStream,
    expected_path: &str,
    pages: LoopbackPages,
) -> Result<LoopbackCallback, LoopbackError> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 512];
    loop {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| LoopbackError::Malformed(e.to_string()))?;
        if n == 0 {
            return Err(LoopbackError::Malformed("connection closed early".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err(LoopbackError::Malformed("request too large".into()));
        }
    }
    let request_text = std::str::from_utf8(&buf)
        .map_err(|e| LoopbackError::Malformed(format!("non-UTF8 request: {e}")))?;
    let first_line = request_text
        .split("\r\n")
        .next()
        .ok_or_else(|| LoopbackError::Malformed("empty request".into()))?;
    let (path, query) = split_request_line(first_line)
        .ok_or_else(|| LoopbackError::Malformed(format!("bad request line: {first_line}")))?;
    if path != expected_path {
        let _ = write_404(stream, &pages.not_found).await;
        return Err(LoopbackError::Malformed(format!("unexpected path: {path}")));
    }
    let params = parse_query(query);
    let callback = if let Some(error) = find_param(&params, "error") {
        let description = find_param(&params, "error_description");
        LoopbackCallback::Error { error, description }
    } else if let Some(code) = find_param(&params, "code") {
        let state = find_param(&params, "state");
        LoopbackCallback::Code { code, state }
    } else {
        let _ = write_400(stream, &pages.bad_request).await;
        return Err(LoopbackError::Malformed(
            "neither code nor error in redirect query".into(),
        ));
    };
    let page = if callback_is_success(&callback) {
        &pages.success
    } else {
        &pages.failure
    };
    let _ = write_ok(stream, page).await;
    Ok(callback)
}

fn callback_is_success(callback: &LoopbackCallback) -> bool {
    matches!(callback, LoopbackCallback::Code { .. })
}

async fn write_ok(
    stream: tokio::net::TcpStream,
    page: &LoopbackPageCopy,
) -> Result<(), LoopbackError> {
    let body = confirmation_page(page);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let mut s = stream;
    s.write_all(response.as_bytes())
        .await
        .map_err(|e| LoopbackError::Bind(e.to_string()))?;
    s.shutdown()
        .await
        .map_err(|e| LoopbackError::Bind(e.to_string()))?;
    Ok(())
}

async fn write_400(stream: tokio::net::TcpStream, message: &str) -> Result<(), LoopbackError> {
    let body = format!(
        "<!doctype html><html><body>{}</body></html>\n",
        html_text(message)
    );
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let mut s = stream;
    s.write_all(response.as_bytes())
        .await
        .map_err(|e| LoopbackError::Bind(e.to_string()))?;
    s.shutdown().await.ok();
    Ok(())
}

async fn write_404(stream: tokio::net::TcpStream, message: &str) -> Result<(), LoopbackError> {
    let body = format!(
        "<!doctype html><html><body>{}</body></html>\n",
        html_text(message)
    );
    let response = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let mut s = stream;
    s.write_all(response.as_bytes())
        .await
        .map_err(|e| LoopbackError::Bind(e.to_string()))?;
    s.shutdown().await.ok();
    Ok(())
}

fn confirmation_page(page: &LoopbackPageCopy) -> String {
    let title = html_text(&page.title);
    let heading = html_text(&page.heading);
    let body = html_text(&page.body);
    let footer = html_text(&page.footer);
    let accent = page.accent;
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title><style>body{{font:14px -apple-system,system-ui,sans-serif;margin:40px auto;max-width:560px;color:#1f1f1f;background:#fff}}h1{{color:{accent};margin:0 0 12px;font-size:20px}}p{{line-height:1.45;margin:0 0 8px}}.muted{{color:#666;font-size:12px;margin-top:24px}}</style></head><body><h1>{heading}</h1><p>{body}</p><p class=\"muted\">{footer}</p></body></html>\n"
    )
}

fn html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn split_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" && method != "POST" {
        return None;
    }
    let target = parts.next()?;
    let (path, query) = match target.find('?') {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => (target, ""),
    };
    let path = path.trim_end_matches('?');
    Some((path, query))
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = match pair.find('=') {
                Some(i) => (&pair[..i], &pair[i + 1..]),
                None => (pair, ""),
            };
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn find_param(params: &[(String, String)], name: &str) -> Option<String> {
    params
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_digit(bytes[i + 1]);
            let lo = hex_digit(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn shuffle(ports: &mut [u16]) {
    use rand::seq::SliceRandom;
    let mut rng = rand::rng();
    ports.shuffle(&mut rng);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::time::Duration;

    fn short_timeout() -> Duration {
        Duration::from_secs(2)
    }

    #[test]
    fn localized_pages_use_copy_resolved_by_the_caller() {
        let mut resolved_keys = Vec::new();
        let pages = LoopbackPages::localized(|message| {
            resolved_keys.push(message.key().expect("keyed page copy").to_string());
            format!("translated:{}", resolved_keys.len())
        });
        assert_eq!(pages.success.heading, "translated:2");
        assert_eq!(pages.failure.heading, "translated:6");
        assert_eq!(pages.bad_request, "translated:9");
        assert_eq!(pages.not_found, "translated:10");
        assert_eq!(resolved_keys.len(), 10);
        assert!(
            resolved_keys.contains(&"services.security.loopback.pages.success.body".to_string())
        );
    }

    #[tokio::test]
    async fn captures_code_callback_with_matching_state() {
        let server = LoopbackServer::bind_to_port(0, "/auth", "state-abc", short_timeout())
            .await
            .unwrap();
        let port = server.port();
        // Concurrent: fire the browser-side request after binding.
        let url = format!("http://127.0.0.1:{port}/auth?code=CODE123&state=state-abc");
        let request_task = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(
                    "GET /auth?code=CODE123&state=state-abc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
                .await
                .unwrap();
            response
        });
        let callback = server.wait_for_callback().await.unwrap();
        let response = request_task.await.unwrap();
        match callback {
            LoopbackCallback::Code { code, state } => {
                assert_eq!(code, "CODE123");
                assert_eq!(state.as_deref(), Some("state-abc"));
            }
            other => panic!("expected code callback, got {other:?}"),
        }
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Sign-in complete"));
        let _ = url; // silence unused warning if branch changes later
    }

    #[tokio::test]
    async fn captures_error_callback_and_returns_error_page() {
        let server = LoopbackServer::bind_to_port(0, "/auth", "state-abc", short_timeout())
            .await
            .unwrap();
        let port = server.port();
        let response_task = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(
                    b"GET /auth?error=access_denied&error_description=user%20cancelled HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let mut response = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
                .await
                .unwrap();
            response
        });
        let callback = server.wait_for_callback().await.unwrap();
        let response = response_task.await.unwrap();
        match callback {
            LoopbackCallback::Error { error, description } => {
                assert_eq!(error, "access_denied");
                assert_eq!(description.as_deref(), Some("user cancelled"));
            }
            other => panic!("expected error callback, got {other:?}"),
        }
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Sign-in failed"));
    }

    #[tokio::test]
    async fn wait_for_callback_times_out_after_idle_window() {
        let server = LoopbackServer::bind_to_port(0, "/auth", "state", Duration::from_millis(150))
            .await
            .unwrap();
        let result = server.wait_for_callback().await;
        assert!(matches!(result, Err(LoopbackError::Timeout(_))));
    }

    #[tokio::test]
    async fn rejects_query_missing_both_code_and_error() {
        let server = LoopbackServer::bind_to_port(0, "/auth", "state", short_timeout())
            .await
            .unwrap();
        let port = server.port();
        let response_task = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(b"GET /auth?state=just-state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
                .await
                .unwrap();
            response
        });
        let result = server.wait_for_callback().await;
        let response = response_task.await.unwrap();
        assert!(matches!(result, Err(LoopbackError::Malformed(_))));
        assert!(response.starts_with("HTTP/1.1 400"));
    }

    #[tokio::test]
    async fn rejects_request_to_wrong_path() {
        let server = LoopbackServer::bind_to_port(0, "/auth", "state", short_timeout())
            .await
            .unwrap();
        let port = server.port();
        let response_task = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(
                    b"GET /not-auth HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let mut response = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
                .await
                .unwrap();
            response
        });
        let result = server.wait_for_callback().await;
        let response = response_task.await.unwrap();
        assert!(matches!(result, Err(LoopbackError::Malformed(_))));
        assert!(response.starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn parses_get_request_with_query() {
        let (path, q) = split_request_line("GET /auth?code=abc&state=xyz HTTP/1.1").unwrap();
        assert_eq!(path, "/auth");
        assert_eq!(q, "code=abc&state=xyz");
    }

    #[test]
    fn parses_post_request() {
        let (path, q) = split_request_line("POST /auth?code=a HTTP/1.1").unwrap();
        assert_eq!(path, "/auth");
        assert_eq!(q, "code=a");
    }

    #[test]
    fn rejects_non_get_post() {
        assert!(split_request_line("PUT /auth HTTP/1.1").is_none());
        assert!(split_request_line("DELETE /auth?code=a HTTP/1.1").is_none());
    }

    #[test]
    fn query_parser_returns_keys_and_decoded_values() {
        let params = parse_query("code=hello%20world&state=x%2Fy");
        assert_eq!(
            params,
            vec![
                ("code".to_string(), "hello world".to_string()),
                ("state".to_string(), "x/y".to_string()),
            ]
        );
    }
}
