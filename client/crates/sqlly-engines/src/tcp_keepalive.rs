//! TCP keepalive for database sockets.
//!
//! Firewalls, NAT gateways and cloud load balancers drop a TCP connection
//! that sits idle for a few minutes, usually without telling either end
//! (Azure's load balancer does it after 4 minutes). A warm pooled client can
//! sit idle for up to five minutes between queries, so without keepalives the next query on it is written into a connection
//! that no longer exists and waits for a reply that never comes.
//!
//! Every SQL Server, PostgreSQL and MySQL socket therefore sends a keepalive
//! after [`IDLE`] of silence, which keeps those middleboxes from forgetting
//! the connection, and gives up after [`RETRIES`] unanswered probes
//! [`INTERVAL`] apart, so a connection that really died fails fast and the
//! query layer's stale-warm retry opens a fresh one.

use std::time::Duration;

/// Silence before the first keepalive probe.
pub const IDLE: Duration = Duration::from_secs(60);

/// Time between unanswered probes.
pub const INTERVAL: Duration = Duration::from_secs(10);

/// Unanswered probes before the operating system closes the connection.
pub const RETRIES: u32 = 3;

/// The keepalive settings every database socket uses.
pub fn params() -> socket2::TcpKeepalive {
    socket2::TcpKeepalive::new()
        .with_time(IDLE)
        .with_interval(INTERVAL)
        .with_retries(RETRIES)
}

/// Turns keepalives on for a socket the app opened itself (SQL Server; the
/// PostgreSQL and MySQL drivers open their own and take the same values
/// through their connection options).
pub fn apply(stream: &tokio::net::TcpStream) -> std::io::Result<()> {
    socket2::SockRef::from(stream).set_tcp_keepalive(&params())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applied_sockets_probe_after_a_minute_of_silence() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        apply(&stream).unwrap();
        let socket = socket2::SockRef::from(&stream);
        assert!(socket.keepalive().unwrap());
        // socket2 only reads the timings back on Unix; Windows sets them
        // through WSAIoctl and offers no getter.
        #[cfg(not(windows))]
        {
            assert_eq!(socket.tcp_keepalive_time().unwrap(), IDLE);
            assert_eq!(socket.tcp_keepalive_interval().unwrap(), INTERVAL);
        }
    }
}
