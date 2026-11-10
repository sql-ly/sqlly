//! Redis Sentinel profile settings: the master name a Sentinel group
//! watches, the sentinels to ask, an optional Sentinel password, and whether
//! read-only scripts may run on a replica.
//!
//! A profile with [`RedisSentinelConfig`] set does not dial `server:port`
//! for data; the Redis driver asks each sentinel in order for the current
//! master's address and connects there. The profile's own ACL user and
//! password authenticate to the master (and replicas); the Sentinel
//! password, when set, authenticates to the sentinels only.

use serde::{Deserialize, Serialize};

use crate::connection::SecretRef;

/// The port a sentinel listens on when a host names none.
pub const DEFAULT_SENTINEL_PORT: u16 = 26379;

/// How a Redis profile reaches its master through Sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RedisSentinelConfig {
    /// The name the sentinels monitor the master under (`mymaster`).
    pub master_name: String,
    /// The sentinels to ask, in order, each `host[:port]` (port 26379 when
    /// omitted, `[v6]:port` for IPv6 literals).
    #[serde(default)]
    pub sentinels: Vec<String>,
    /// The password the sentinels themselves require (`requirepass` on the
    /// sentinel), when different from the data nodes'. Absent when the
    /// sentinels are open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentinel_password_ref: Option<SecretRef>,
    /// Route read-only scripts to a replica the sentinels report healthy,
    /// falling back to the master when there is none.
    #[serde(default)]
    pub prefer_replica_reads: bool,
}

impl RedisSentinelConfig {
    /// The sentinel list as `(host, port)` pairs, skipping blanks. A host
    /// without a port gets [`DEFAULT_SENTINEL_PORT`].
    pub fn sentinel_addresses(&self) -> Result<Vec<(String, u16)>, String> {
        self.sentinels
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(parse_sentinel_host)
            .collect()
    }

    pub fn problem_message(&self) -> Option<crate::DisplayMessage> {
        if self.master_name.trim().is_empty() {
            return Some(crate::DisplayMessage::new(
                "services.redis.sentinel.missing_master",
                "A Sentinel connection needs the master name the sentinels watch",
            ));
        }
        match self.sentinel_addresses() {
            Err(error) => Some(sentinel_parse_message(&error)),
            Ok(list) if list.is_empty() => Some(crate::DisplayMessage::new(
                "services.redis.sentinel.missing_hosts",
                "A Sentinel connection needs at least one sentinel host",
            )),
            Ok(_) => None,
        }
    }

    /// English compatibility fallback; localized callers use [`Self::problem_message`].
    pub fn problem(&self) -> Option<String> {
        if self.master_name.trim().is_empty() {
            return Some("A Sentinel connection needs the master name the sentinels watch".into());
        }
        match self.sentinel_addresses() {
            Err(e) => Some(e),
            Ok(list) if list.is_empty() => {
                Some("A Sentinel connection needs at least one sentinel host".into())
            }
            Ok(_) => None,
        }
    }

    /// The copy an export bundle or credential file carries: everything but
    /// the Sentinel password reference.
    pub fn without_secret(&self) -> Self {
        Self {
            sentinel_password_ref: None,
            ..self.clone()
        }
    }
}

/// Split a comma- (or whitespace-) separated sentinel list as the editor
/// field holds it into one entry per host.
pub fn split_sentinel_list(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n', '\t', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn sentinel_parse_message(error: &str) -> crate::DisplayMessage {
    let quoted = |index: usize| error.split('"').nth(index).unwrap_or_default().to_string();
    if error.contains("is not a sentinel port") {
        crate::DisplayMessage::new("services.redis.sentinel.invalid_port", error)
            .with_arg("port", quoted(1))
            .with_arg("host", quoted(3))
    } else if error.contains("unclosed IPv6 bracket") {
        crate::DisplayMessage::new("services.redis.sentinel.unclosed_ipv6", error)
            .with_arg("host", quoted(1))
    } else {
        crate::DisplayMessage::raw(error)
    }
}

/// `host`, `host:port`, `[v6]`, or `[v6]:port` → `(host, port)`.
pub fn parse_sentinel_host(text: &str) -> Result<(String, u16), String> {
    let text = text.trim();
    let bad_port = |p: &str| format!("\"{p}\" is not a sentinel port (in \"{text}\")");
    if let Some(rest) = text.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err(format!("\"{text}\" has an unclosed IPv6 bracket"));
        };
        let host = rest[..end].to_string();
        let after = &rest[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| bad_port(p))?,
            None if after.is_empty() => DEFAULT_SENTINEL_PORT,
            None => return Err(bad_port(after)),
        };
        return Ok((host, port));
    }
    match text.rsplit_once(':') {
        // A bare IPv6 literal (more than one colon) has no port.
        Some((host, _)) if host.contains(':') => Ok((text.to_string(), DEFAULT_SENTINEL_PORT)),
        Some((host, port)) => {
            if host.is_empty() {
                return Err(format!("\"{text}\" has no host"));
            }
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| bad_port(port))?;
            Ok((host.to_string(), port))
        }
        None => Ok((text.to_string(), DEFAULT_SENTINEL_PORT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::SecretProvider;

    fn sample() -> RedisSentinelConfig {
        RedisSentinelConfig {
            master_name: "mymaster".into(),
            sentinels: vec![
                "10.0.0.1:26379".into(),
                "sentinel-b".into(),
                "[::1]:26381".into(),
            ],
            sentinel_password_ref: Some(SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: "4b5c0b51-4f5e-4f0b-9b55-7f1d57b8a8a1".into(),
            }),
            prefer_replica_reads: true,
        }
    }

    #[test]
    fn redis_sentinel_serde_round_trip() {
        let config = sample();
        let json = serde_json::to_string(&config).unwrap();
        let back: RedisSentinelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn redis_sentinel_minimal_json_defaults() {
        let back: RedisSentinelConfig =
            serde_json::from_str(r#"{"master_name":"mymaster"}"#).unwrap();
        assert_eq!(back.master_name, "mymaster");
        assert!(back.sentinels.is_empty());
        assert!(back.sentinel_password_ref.is_none());
        assert!(!back.prefer_replica_reads);
        // No password reference → the key is omitted entirely.
        let json = serde_json::to_string(&back).unwrap();
        assert!(!json.contains("sentinel_password_ref"), "{json}");
    }

    #[test]
    fn redis_sentinel_addresses_default_the_port() {
        assert_eq!(
            sample().sentinel_addresses().unwrap(),
            vec![
                ("10.0.0.1".to_string(), 26379),
                ("sentinel-b".to_string(), DEFAULT_SENTINEL_PORT),
                ("::1".to_string(), 26381),
            ]
        );
        assert_eq!(
            parse_sentinel_host("fe80::1").unwrap(),
            ("fe80::1".to_string(), DEFAULT_SENTINEL_PORT)
        );
        assert_eq!(
            parse_sentinel_host("[fe80::1]").unwrap(),
            ("fe80::1".to_string(), DEFAULT_SENTINEL_PORT)
        );
        assert!(parse_sentinel_host("host:abc").is_err());
        assert!(parse_sentinel_host("host:0").is_err());
        assert!(parse_sentinel_host(":26379").is_err());
    }

    #[test]
    fn redis_sentinel_problems_name_what_is_missing() {
        assert!(sample().problem().is_none());
        let mut c = sample();
        c.master_name = "  ".into();
        assert!(c.problem().unwrap().contains("master name"));
        let mut c = sample();
        c.sentinels = vec![" ".into()];
        assert!(c.problem().unwrap().contains("at least one sentinel"));
        let mut c = sample();
        c.sentinels = vec!["h:99999".into()];
        assert!(c.problem().unwrap().contains("not a sentinel port"));
    }

    #[test]
    fn redis_sentinel_without_secret_drops_only_the_password() {
        let stripped = sample().without_secret();
        assert!(stripped.sentinel_password_ref.is_none());
        assert_eq!(stripped.master_name, "mymaster");
        assert_eq!(stripped.sentinels.len(), 3);
        assert!(stripped.prefer_replica_reads);
    }

    #[test]
    fn redis_sentinel_list_splits_commas_and_spaces() {
        assert_eq!(
            split_sentinel_list(" a:1, b:2  c ,,"),
            vec!["a:1".to_string(), "b:2".to_string(), "c".to_string()]
        );
    }
}
