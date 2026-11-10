//! SOCKS5 and HTTP CONNECT proxy dialing.

use std::time::Duration;

use async_http_proxy as http_proxy;
use sqlly_core::connection::{ProxyHop, ProxyKind};
use sqlly_core::{StudioError, StudioResult};
use tokio::net::TcpStream;
use tokio_socks::tcp::Socks5Stream;

/// Dial `target_host:target_port` through `hop`, returning a connected TCP stream.
pub async fn dial_through_proxy(
    hop: &ProxyHop,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> StudioResult<TcpStream> {
    let dial = async {
        match hop.kind {
            ProxyKind::Socks5 => dial_socks5(hop, target_host, target_port).await,
            ProxyKind::HttpConnect => dial_http_connect(hop, target_host, target_port).await,
        }
    };
    tokio::time::timeout(timeout, dial).await.map_err(|_| {
        StudioError::Connection(format!(
            "proxy {}:{} timed out after {}s",
            hop.host,
            hop.port,
            timeout.as_secs()
        ))
    })?
}

async fn dial_socks5(
    hop: &ProxyHop,
    target_host: &str,
    target_port: u16,
) -> StudioResult<TcpStream> {
    let proxy_addr = (hop.host.as_str(), hop.port);
    let target_addr = (target_host, target_port);
    let stream = if let Some(username) = hop.username.as_ref().filter(|u| !u.trim().is_empty()) {
        let password = hop
            .password
            .as_ref()
            .map(|r| r.resolve())
            .transpose()?
            .unwrap_or_default();
        Socks5Stream::connect_with_password(proxy_addr, target_addr, username, &password)
            .await
            .map_err(|e| proxy_error(hop, e))?
    } else {
        Socks5Stream::connect(proxy_addr, target_addr)
            .await
            .map_err(|e| proxy_error(hop, e))?
    };
    Ok(stream.into_inner())
}

async fn dial_http_connect(
    hop: &ProxyHop,
    target_host: &str,
    target_port: u16,
) -> StudioResult<TcpStream> {
    let mut stream = TcpStream::connect((hop.host.as_str(), hop.port))
        .await
        .map_err(|e| proxy_error(hop, e))?;
    if let Some(username) = hop.username.as_ref().filter(|u| !u.trim().is_empty()) {
        let password = hop
            .password
            .as_ref()
            .map(|r| r.resolve())
            .transpose()?
            .unwrap_or_default();
        http_proxy::http_connect_tokio_with_basic_auth(
            &mut stream,
            target_host,
            target_port,
            username,
            &password,
        )
        .await
        .map_err(|e| proxy_error(hop, e))?;
    } else {
        http_proxy::http_connect_tokio(&mut stream, target_host, target_port)
            .await
            .map_err(|e| proxy_error(hop, e))?;
    }
    Ok(stream)
}

fn proxy_error(hop: &ProxyHop, err: impl std::fmt::Display) -> StudioError {
    StudioError::Connection(format!("proxy {}:{}: {err}", hop.host, hop.port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::connection::ProxyKind;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn http_connect_dial_round_trips_through_local_stub() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("CONNECT echo.test:9999 "));
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let mut echo_buf = [0u8; 16];
            let n = socket.read(&mut echo_buf).await.unwrap();
            socket.write_all(&echo_buf[..n]).await.unwrap();
        });

        let hop = ProxyHop {
            kind: ProxyKind::HttpConnect,
            host: "127.0.0.1".to_string(),
            port: proxy_port,
            username: None,
            password: None,
        };
        let mut stream = dial_through_proxy(&hop, "echo.test", 9999, Duration::from_secs(5))
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_dial_round_trips_through_local_stub() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            socket.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [0x05, 0x01, 0x00]);
            socket.write_all(&[0x05, 0x00]).await.unwrap();
            let mut req = [0u8; 10];
            socket.read_exact(&mut req).await.unwrap();
            assert_eq!(req[0], 0x05);
            assert_eq!(req[1], 0x01);
            socket
                .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0x27, 0x0f])
                .await
                .unwrap();
            let mut echo_buf = [0u8; 16];
            let n = socket.read(&mut echo_buf).await.unwrap();
            socket.write_all(&echo_buf[..n]).await.unwrap();
        });

        let hop = ProxyHop {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".to_string(),
            port: proxy_port,
            username: None,
            password: None,
        };
        let mut stream = dial_through_proxy(&hop, "127.0.0.1", 9999, Duration::from_secs(5))
            .await
            .unwrap();
        stream.write_all(b"pong").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
        server.await.unwrap();
    }
}
