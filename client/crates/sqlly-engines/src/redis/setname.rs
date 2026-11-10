//! `CLIENT SETNAME` for every connection SQLly opens.
//!
//! Each connection — the query lane, the Profiler's `MONITOR` connection and
//! the Pub/Sub listener — names itself `sqlly:<profile-slug>:<pid>`, so
//! `CLIENT LIST` (and the Activity Monitor's Sessions tab) shows which
//! connections are SQLly's, which saved connection opened them, and which
//! app process holds them. A server or ACL rule that refuses the command
//! leaves the connection unnamed; the refusal is never an error.

use sqlly_core::ConnectionProfile;

use super::RedisClient;

/// The longest slug kept from a profile name; Redis has no hard limit, but a
/// `CLIENT LIST` line stays readable.
const MAX_SLUG: usize = 40;

/// A profile name as a client-name segment: lower-case ASCII letters and
/// digits, every other run of characters one `-`, trimmed, at most
/// [`MAX_SLUG`] characters, `connection` when nothing is left. Redis refuses
/// names with spaces or control characters, and `:` separates the segments.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(MAX_SLUG));
    let mut dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            dash = true;
        }
        if out.len() >= MAX_SLUG {
            break;
        }
    }
    out.truncate(MAX_SLUG);
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "connection".to_string()
    } else {
        out
    }
}

/// `sqlly:<profile-slug>:<pid>` for the profile.
pub fn client_name(profile: &ConnectionProfile) -> String {
    format!("sqlly:{}:{}", slug(&profile.name), std::process::id())
}

/// The `CLIENT SETNAME` command for the profile.
pub(crate) fn setname_cmd(profile: &ConnectionProfile) -> redis::Cmd {
    let mut cmd = redis::cmd("CLIENT");
    cmd.arg("SETNAME").arg(client_name(profile));
    cmd
}

/// Name an open query connection, ignoring a refusal (an ACL without
/// `client|setname`, a proxy that does not forward it).
pub(crate) async fn name_connection(client: &RedisClient, profile: &ConnectionProfile) {
    if let Err(error) = client.run(&setname_cmd(profile)).await {
        tracing::debug!(%error, "CLIENT SETNAME refused; the connection stays unnamed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    #[test]
    fn slugs_are_safe_client_name_segments() {
        assert_eq!(slug("Prod Cache (EU)"), "prod-cache-eu");
        assert_eq!(slug("  redis://h:6379/2 "), "redis-h-6379-2");
        assert_eq!(slug("Ünïcode 名前"), "n-code");
        assert_eq!(slug("::"), "connection");
        assert_eq!(slug(""), "connection");
        let long = "a".repeat(100);
        assert_eq!(slug(&long).len(), MAX_SLUG);
        assert!(
            slug("a b c d e f g h i j k l m n o p q r s t u v w x y z")
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-')
        );
    }

    #[test]
    fn client_name_carries_profile_and_process() {
        let profile = ConnectionProfile::sql_login(
            "Orders Cache",
            "h",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        let name = client_name(&profile);
        assert_eq!(name, format!("sqlly:orders-cache:{}", std::process::id()));
        assert!(!name.contains(' '));
        let cmd = setname_cmd(&profile);
        let packed = String::from_utf8_lossy(&cmd.get_packed_command()).into_owned();
        assert!(packed.contains("SETNAME") && packed.contains(&name));
    }

    /// Live: the query connection, the Profiler's MONITOR connection and the
    /// Pub/Sub listener all appear in `CLIENT LIST` under the SQLly name.
    /// Opt-in through `SQLLY_REDIS_URL`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_every_connection_is_named() {
        let Some(mut profile) = super::super::live_profile_from_env("wsj setname") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        profile.database = Some("15".into());
        let name = client_name(&profile);
        let client = super::super::open_client(&profile).await.expect("client");
        let mut monitor = super::super::live::open_monitor(&profile)
            .await
            .expect("monitor");
        let _pubsub = super::super::live::open_pubsub(
            &profile,
            &super::super::live::parse_subscribe_targets("wsj:setname"),
        )
        .await
        .expect("pubsub");
        let mut getname = redis::cmd("CLIENT");
        getname.arg("GETNAME");
        let own = client.run(&getname).await.unwrap();
        assert_eq!(
            own,
            redis::Value::BulkString(name.clone().into_bytes()),
            "query connection name"
        );
        // The listing is read from the query connection; the monitor sees
        // the command, which proves its own stream is live too.
        let mut list = redis::cmd("CLIENT");
        list.arg("LIST");
        let listing = match client.run(&list).await.unwrap() {
            redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
            redis::Value::VerbatimString { text, .. } => text,
            other => panic!("{other:?}"),
        };
        let named: Vec<&str> = listing
            .lines()
            .filter(|l| l.contains(&format!("name={name} ")))
            .collect();
        // Redis, Valkey and KeyDB say which connection is which; DragonflyDB
        // lists no `cmd=` field, so there the count is the evidence.
        if listing.contains(" cmd=") {
            assert!(
                named
                    .iter()
                    .any(|l| l.contains("cmd=monitor") || l.contains("flags=O")),
                "monitor connection named: {named:?}"
            );
            assert!(
                named
                    .iter()
                    .any(|l| l.contains("sub=1") || l.contains("cmd=subscribe")),
                "pubsub connection named: {named:?}"
            );
        }
        assert!(named.len() >= 3, "{named:?}");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), monitor.next_frame())
            .await
            .expect("monitor frame");
        assert!(frame.is_some());
    }
}
