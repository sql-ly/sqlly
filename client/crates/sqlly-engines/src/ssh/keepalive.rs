//! SSH keepalives and dropped-session detection for tunnels.
//!
//! Every SSH session a tunnel opens sends a keepalive request after
//! `interval` of silence from the server (30 s unless the profile says
//! otherwise) and gives up after `max_missed` go unanswered, so an idle
//! tunnel survives NAT and firewall idle timeouts, and a dead one is noticed
//! instead of hanging every connection made through it. A session that ends
//! — keepalives unanswered, the server disconnected, the network dropped —
//! marks its tunnel closed; the [`super::TunnelManager`] then dials a fresh
//! tunnel the next time a connection asks for one, and the engine that asked
//! can tell the user the tunnel was re-established. Nothing is re-run on the
//! user's behalf: the command that hit the dead tunnel fails, and the user
//! runs it again.

use std::sync::Arc;

use russh::client;
use sqlly_core::connection::SshKeepalive;
use tokio_util::sync::CancellationToken;

/// The russh client configuration for a tunnel's sessions.
pub fn client_config(keepalive: &SshKeepalive) -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: keepalive.interval(),
        keepalive_max: keepalive.max_missed(),
        ..client::Config::default()
    })
}

/// Set when any SSH session of a tunnel has ended. Cloned into each
/// session's handler, which trips it from `disconnected`; the tunnel's
/// accept loop and the manager's reuse check read it.
#[derive(Debug, Clone, Default)]
pub struct SessionClosed(CancellationToken);

impl SessionClosed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the tunnel's session as ended.
    pub fn trip(&self) {
        self.0.cancel();
    }

    pub fn is_tripped(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Resolves once the session has ended.
    pub async fn tripped(&self) {
        self.0.cancelled().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn default_profiles_send_a_keepalive_every_30_seconds_and_allow_3_misses() {
        let config = client_config(&SshKeepalive::default());
        assert_eq!(config.keepalive_interval, Some(Duration::from_secs(30)));
        assert_eq!(config.keepalive_max, 3);
    }

    #[test]
    fn the_profile_sets_the_interval_and_the_miss_limit_and_0_turns_keepalives_off() {
        let config = client_config(&SshKeepalive {
            interval_seconds: Some(10),
            max_missed: Some(5),
        });
        assert_eq!(config.keepalive_interval, Some(Duration::from_secs(10)));
        assert_eq!(config.keepalive_max, 5);
        let off = client_config(&SshKeepalive {
            interval_seconds: Some(0),
            max_missed: Some(0),
        });
        assert_eq!(off.keepalive_interval, None);
        // A miss limit of 0 would mean "never give up" to russh; at least
        // one miss closes the session.
        assert_eq!(off.keepalive_max, 1);
    }

    #[test]
    fn keepalive_settings_round_trip_and_stay_out_of_saved_profiles_by_default() {
        let json = serde_json::to_string(&SshKeepalive::default()).unwrap();
        assert_eq!(json, "{}");
        let custom = SshKeepalive {
            interval_seconds: Some(15),
            max_missed: None,
        };
        let json = serde_json::to_string(&custom).unwrap();
        assert_eq!(serde_json::from_str::<SshKeepalive>(&json).unwrap(), custom);
    }

    #[tokio::test]
    async fn a_tripped_session_resolves_its_waiters() {
        let closed = SessionClosed::new();
        let waiter = closed.clone();
        let wait = tokio::spawn(async move { waiter.tripped().await });
        assert!(!closed.is_tripped());
        closed.trip();
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .expect("woken")
            .unwrap();
        assert!(closed.is_tripped());
    }
}
