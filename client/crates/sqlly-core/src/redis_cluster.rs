//! Redis Cluster settings on a connection profile.
//!
//! A Redis profile can point at a single server or at a Redis Cluster
//! (self-hosted, ElastiCache cluster mode, MemoryDB). [`RedisClusterMode`]
//! picks how the driver connects: `Auto` asks the server on connect whether
//! cluster mode is on and remembers the answer for the session, the other
//! two force one answer. The node map rewrites the addresses cluster nodes
//! advertise (in `CLUSTER SLOTS` / `MOVED` replies) into addresses this
//! machine can dial, which is what a cluster reached through NAT, a port
//! mapping or SSH forwards needs: the nodes announce their private
//! addresses, and the client must dial something else.
//!
//! The seed list is the profile's `server` field, which for Redis may hold
//! a comma-separated list (`10.0.0.1:7000, 10.0.0.2:7001`); entries without
//! a port use the profile's port. [`seed_nodes`] reads it.

use serde::{Deserialize, Serialize};

use crate::ConnectionProfile;

/// How a Redis profile connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisClusterMode {
    /// Ask the server on connect (`INFO cluster`) and remember the answer
    /// for the session.
    #[default]
    Auto,
    /// Always one server; never route by slot.
    Standalone,
    /// Always a cluster: route every command to the node owning its slot.
    Cluster,
}

impl RedisClusterMode {
    pub const ALL: [RedisClusterMode; 3] = [Self::Auto, Self::Standalone, Self::Cluster];

    /// The label the connection editor shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Detect automatically",
            Self::Standalone => "Single server",
            Self::Cluster => "Redis Cluster",
        }
    }

    pub fn message(self) -> crate::DisplayMessage {
        let (key, fallback) = match self {
            Self::Auto => ("services.redis.cluster.mode_auto", "Detect automatically"),
            Self::Standalone => ("services.redis.cluster.mode_standalone", "Single server"),
            Self::Cluster => ("services.redis.cluster.mode_cluster", "Redis Cluster"),
        };
        crate::DisplayMessage::new(key, fallback)
    }
}

/// One node-map entry: the `host:port` a node advertises and the
/// `host:port` to dial instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedisNodeMapping {
    pub advertised: String,
    pub dial: String,
}

/// The cluster settings of a Redis profile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RedisClusterConfig {
    #[serde(default)]
    pub mode: RedisClusterMode,
    /// Advertised → dial address rewrites, applied to every node the
    /// cluster names (seeds included).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_map: Vec<RedisNodeMapping>,
}

impl RedisClusterConfig {
    /// True when the settings say nothing beyond the defaults, so the
    /// profile can drop them.
    pub fn is_default(&self) -> bool {
        self.mode == RedisClusterMode::Auto && self.node_map.is_empty()
    }

    /// The dial address for an advertised `host:port`, when the map names
    /// it. Host comparison ignores ASCII case and IPv6 brackets.
    pub fn dial_for(&self, host: &str, port: u16) -> Option<(String, u16)> {
        let want_host = strip_brackets(host);
        self.node_map.iter().find_map(|entry| {
            let (h, p) = split_host_port(&entry.advertised, 0)?;
            if p == port && h.eq_ignore_ascii_case(want_host) {
                split_host_port(&entry.dial, port).map(|(h, p)| (h.to_string(), p))
            } else {
                None
            }
        })
    }
}

/// The mode a profile connects with: its setting, `Auto` when it has none.
pub fn cluster_mode(profile: &ConnectionProfile) -> RedisClusterMode {
    profile
        .redis_cluster
        .as_ref()
        .map(|c| c.mode)
        .unwrap_or_default()
}

/// Parse the node-map text the connection editor holds: one
/// `advertised => dial` (or `advertised dial`, `advertised=dial`) pair per
/// line or per comma-separated entry. Blank entries are skipped; an entry
/// that is not two `host:port` addresses is an error naming it.
pub fn parse_node_map(text: &str) -> Result<Vec<RedisNodeMapping>, String> {
    let mut out = Vec::new();
    for entry in text
        .split(['\n', ','])
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let parts: Vec<&str> = if let Some((a, b)) = entry.split_once("=>") {
            vec![a.trim(), b.trim()]
        } else if let Some((a, b)) = entry.split_once("->") {
            vec![a.trim(), b.trim()]
        } else if let Some((a, b)) = entry.split_once('=') {
            vec![a.trim(), b.trim()]
        } else {
            entry.split_whitespace().collect()
        };
        let [advertised, dial] = parts.as_slice() else {
            return Err(format!(
                "\"{entry}\" is not \"advertised host:port => dial host:port\""
            ));
        };
        for address in [advertised, dial] {
            match split_host_port(address, 0) {
                Some((host, port)) if !host.is_empty() && port != 0 => {}
                _ => return Err(format!("\"{address}\" is not a host:port address")),
            }
        }
        out.push(RedisNodeMapping {
            advertised: advertised.to_string(),
            dial: dial.to_string(),
        });
    }
    Ok(out)
}

/// The node map as editor text, one `advertised => dial` per line.
pub fn node_map_text(map: &[RedisNodeMapping]) -> String {
    map.iter()
        .map(|m| format!("{} => {}", m.advertised, m.dial))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The seed nodes a profile names: its `server` field split on commas,
/// each entry `host`, `host:port` or `[v6]:port`; an entry without a port
/// takes the profile's port (6379 when that is 0). Blank entries are
/// dropped.
pub fn seed_nodes(profile: &ConnectionProfile) -> Vec<(String, u16)> {
    let default_port = if profile.port == 0 {
        6379
    } else {
        profile.port
    };
    profile
        .server
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| split_host_port(entry, default_port))
        .map(|(h, p)| (h.to_string(), p))
        .collect()
}

/// True when the profile's `server` names more than one host.
pub fn has_host_list(profile: &ConnectionProfile) -> bool {
    profile.server.contains(',')
}

fn strip_brackets(host: &str) -> &str {
    host.trim().trim_start_matches('[').trim_end_matches(']')
}

/// `host[:port]` / `[v6][:port]`; a bare IPv6 address (more than one
/// colon, no brackets) is a host without a port.
pub fn split_host_port(text: &str, default_port: u16) -> Option<(&str, u16)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let after = rest[end + 1..].trim_start_matches(':');
        let port = if after.is_empty() {
            default_port
        } else {
            after.parse().ok()?
        };
        return Some((host, port));
    }
    if text.matches(':').count() > 1 {
        return Some((text, default_port));
    }
    match text.rsplit_once(':') {
        Some((host, port)) => Some((host.trim(), port.trim().parse().ok()?)),
        None => Some((text, default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SecretProvider, SecretRef};

    fn profile(server: &str, port: u16) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "c",
            server,
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.port = port;
        p
    }

    #[test]
    fn redis_cluster_config_round_trips_through_serde_and_defaults_to_auto() {
        let config = RedisClusterConfig {
            mode: RedisClusterMode::Cluster,
            node_map: vec![RedisNodeMapping {
                advertised: "10.0.0.5:7001".into(),
                dial: "127.0.0.1:17001".into(),
            }],
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"cluster\""), "{json}");
        let back: RedisClusterConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
        let empty: RedisClusterConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.mode, RedisClusterMode::Auto);
        assert!(empty.is_default());
        assert_eq!(RedisClusterMode::default(), RedisClusterMode::Auto);
    }

    #[test]
    fn redis_cluster_profile_field_is_optional_and_round_trips() {
        let mut p = profile("h", 7000);
        assert_eq!(cluster_mode(&p), RedisClusterMode::Auto);
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("redis_cluster"), "absent when unset");
        p.redis_cluster = Some(RedisClusterConfig {
            mode: RedisClusterMode::Standalone,
            node_map: Vec::new(),
        });
        let json = serde_json::to_string(&p).unwrap();
        let back: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.redis_cluster, p.redis_cluster);
        assert_eq!(cluster_mode(&back), RedisClusterMode::Standalone);
    }

    #[test]
    fn redis_cluster_node_map_parses_lines_and_maps_dial_addresses() {
        let map = parse_node_map(
            "10.0.0.5:7001 => 127.0.0.1:17001\n10.0.0.6:7002 -> 127.0.0.1:17002, [fd00::1]:7003=localhost:17003\n\n",
        )
        .unwrap();
        assert_eq!(map.len(), 3);
        let config = RedisClusterConfig {
            mode: RedisClusterMode::Cluster,
            node_map: map.clone(),
        };
        assert_eq!(
            config.dial_for("10.0.0.5", 7001),
            Some(("127.0.0.1".into(), 17001))
        );
        assert_eq!(
            config.dial_for("fd00::1", 7003),
            Some(("localhost".into(), 17003))
        );
        assert_eq!(config.dial_for("10.0.0.5", 7002), None);
        assert_eq!(
            parse_node_map(&node_map_text(&map)).unwrap(),
            map,
            "editor text round-trips"
        );
        assert!(parse_node_map("10.0.0.5:7001").is_err());
        assert!(parse_node_map("a => b").is_err());
    }

    #[test]
    fn redis_cluster_seed_nodes_split_the_server_list() {
        let p = profile("10.0.0.1:7000, 10.0.0.2 ,[::1]:7002", 7100);
        assert_eq!(
            seed_nodes(&p),
            vec![
                ("10.0.0.1".to_string(), 7000),
                ("10.0.0.2".to_string(), 7100),
                ("::1".to_string(), 7002),
            ]
        );
        assert!(has_host_list(&p));
        let single = profile("cache.example.com", 0);
        assert_eq!(
            seed_nodes(&single),
            vec![("cache.example.com".to_string(), 6379)]
        );
        assert!(!has_host_list(&single));
    }
}
