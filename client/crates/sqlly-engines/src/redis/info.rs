//! Parsers for Redis's text-shaped replies (`INFO`, `CLIENT LIST`, the
//! keyspace lines) and the Server Dashboard / Activity Monitor metric
//! derivations built on them.

use std::collections::BTreeMap;

/// One `key:value` line of `INFO`, tagged with its `# Section`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoEntry {
    pub section: String,
    pub key: String,
    pub value: String,
}

/// Parse `INFO` output into entries in server order.
pub fn parse_info(text: &str) -> Vec<InfoEntry> {
    let mut section = String::new();
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('#') {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        entries.push(InfoEntry {
            section: section.clone(),
            key: key.trim().to_string(),
            value: value.trim().to_string(),
        });
    }
    entries
}

/// `INFO` as a key → value map (sections dropped; keys are unique server-wide).
pub fn info_map(text: &str) -> BTreeMap<String, String> {
    parse_info(text)
        .into_iter()
        .map(|e| (e.key, e.value))
        .collect()
}

/// One `INFO keyspace` line: `db0:keys=3,expires=1,avg_ttl=1234[,subexpiry=0]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyspaceLine {
    pub db: i64,
    pub keys: i64,
    pub expires: i64,
    pub avg_ttl_ms: i64,
}

/// Parse a `dbN:` value (`keys=3,expires=1,avg_ttl=1234`).
pub fn parse_keyspace_value(db: i64, value: &str) -> KeyspaceLine {
    let mut line = KeyspaceLine {
        db,
        keys: 0,
        expires: 0,
        avg_ttl_ms: 0,
    };
    for pair in value.split(',') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let n = v.trim().parse::<i64>().unwrap_or(0);
        match k.trim() {
            "keys" => line.keys = n,
            "expires" => line.expires = n,
            "avg_ttl" => line.avg_ttl_ms = n,
            _ => {}
        }
    }
    line
}

/// The keyspace lines of an `INFO` (or `INFO keyspace`) reply, by database.
pub fn parse_keyspace(text: &str) -> Vec<KeyspaceLine> {
    let mut lines: Vec<KeyspaceLine> = parse_info(text)
        .into_iter()
        .filter_map(|entry| {
            let db = entry.key.strip_prefix("db")?.parse::<i64>().ok()?;
            Some(parse_keyspace_value(db, &entry.value))
        })
        .collect();
    lines.sort_by_key(|l| l.db);
    lines
}

/// One `CLIENT LIST` line as its `key=value` fields, in server order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientEntry {
    pub fields: Vec<(String, String)>,
}

impl ClientEntry {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse `CLIENT LIST`. Every field is optional; a relative that omits
/// one simply has no entry for it.
pub fn parse_client_list(text: &str) -> Vec<ClientEntry> {
    text.lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty())
        .map(|line| ClientEntry {
            fields: line
                .split(' ')
                .filter(|p| !p.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => (pair.to_string(), String::new()),
                })
                .collect(),
        })
        .collect()
}

/// The product name and version an `INFO server` reply announces: relatives
/// that publish their own version field (`valkey_version`,
/// `dragonfly_version`, `garnet_version`, `keydb_version`) are named ahead
/// of the Redis compatibility level, e.g. `("Valkey", "9.1.1 (Redis 7.2.4)")`.
pub fn product_and_version(info: &BTreeMap<String, String>) -> (String, String) {
    let redis_version = info.get("redis_version").cloned().unwrap_or_default();
    for (field, name) in [
        ("valkey_version", "Valkey"),
        ("dragonfly_version", "DragonflyDB"),
        ("garnet_version", "Garnet"),
        ("keydb_version", "KeyDB"),
    ] {
        if let Some(own) = info.get(field) {
            let version = if redis_version.is_empty() {
                own.clone()
            } else {
                format!("{own} (Redis {redis_version})")
            };
            return (name.to_string(), version);
        }
    }
    if let Some(server_name) = info.get("server_name")
        && !server_name.eq_ignore_ascii_case("redis")
        && !server_name.is_empty()
    {
        let mut chars = server_name.chars();
        let capitalised = match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        };
        return (capitalised, redis_version);
    }
    ("Redis".to_string(), redis_version)
}

/// The Server Dashboard groups the Redis engine answers, each a named
/// subset of the shared tile keys derived from `INFO` (and `DBSIZE`).
pub const DASHBOARD_GROUPS: &[&str] = &[
    "server",
    "clients",
    "memory",
    "stats",
    "replication",
    "keyspace",
];

fn num(info: &BTreeMap<String, String>, key: &str) -> Option<f64> {
    info.get(key).and_then(|v| v.trim().parse::<f64>().ok())
}

fn fmt(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}")
    }
}

/// The `(metric, value)` rows for one dashboard group. `objects_total` is the
/// key count across every logical database and `indexes_total` the keys
/// with an expiry, both from `INFO keyspace`.
pub fn dashboard_metrics(group: &str, info: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |metric: &str, value: Option<f64>| {
        if let Some(v) = value {
            out.push((metric.to_string(), fmt(v)));
        }
    };
    match group {
        "server" => {
            push("uptime_seconds", num(info, "uptime_in_seconds"));
        }
        "clients" => {
            push("connections_active", num(info, "connected_clients"));
            push(
                "connections_max",
                num(info, "maxclients").or_else(|| num(info, "max_clients")),
            );
            push("sessions_blocked", num(info, "blocked_clients"));
            push(
                "sessions_running",
                num(info, "connected_clients")
                    .zip(num(info, "blocked_clients").or(Some(0.0)))
                    .map(|(c, b)| (c - b).max(0.0)),
            );
        }
        "memory" => {
            push(
                "memory_used_mb",
                num(info, "used_memory").map(|b| b / 1048576.0),
            );
            push(
                "memory_configured_mb",
                num(info, "maxmemory")
                    .filter(|m| *m > 0.0)
                    .or_else(|| num(info, "total_system_memory"))
                    .map(|b| b / 1048576.0),
            );
            push(
                "storage_total_mb",
                num(info, "used_memory_rss").map(|b| b / 1048576.0),
            );
            push(
                "reclaimable_mb",
                num(info, "used_memory_rss")
                    .zip(num(info, "used_memory"))
                    .map(|(rss, used)| (rss - used).max(0.0) / 1048576.0),
            );
        }
        "stats" => {
            push("queries_total", num(info, "total_commands_processed"));
            push(
                "transactions_total",
                num(info, "total_connections_received"),
            );
            push("rows_returned_total", num(info, "total_net_output_bytes"));
            push("slow_queries_total", num(info, "rejected_connections"));
            push("lock_waits_total", num(info, "evicted_keys"));
            push("temp_files_total", num(info, "expired_keys"));
            let hits = num(info, "keyspace_hits");
            let misses = num(info, "keyspace_misses");
            push(
                "cache_hit_ratio",
                hits.zip(misses).map(|(h, m)| {
                    if h + m > 0.0 {
                        100.0 * h / (h + m)
                    } else {
                        100.0
                    }
                }),
            );
        }
        "replication" => {
            let role = info.get("role").map(String::as_str).unwrap_or("master");
            if role == "slave" || role == "replica" {
                // Seconds since the last interaction with the primary.
                push(
                    "replication_lag_seconds",
                    num(info, "master_last_io_seconds_ago").filter(|s| *s >= 0.0),
                );
            } else {
                push(
                    "replication_lag_seconds",
                    num(info, "connected_slaves").map(|_| 0.0),
                );
            }
        }
        "keyspace" => {
            // An empty server has no dbN lines at all; zero keys is the
            // honest answer then, not a missing tile.
            let mut keys = 0.0;
            let mut expires = 0.0;
            for (key, value) in info {
                if let Some(db) = key.strip_prefix("db")
                    && db.parse::<i64>().is_ok()
                {
                    let line = parse_keyspace_value(0, value);
                    keys += line.keys as f64;
                    expires += line.expires as f64;
                }
            }
            push("objects_total", Some(keys));
            push("indexes_total", Some(expires));
        }
        _ => {}
    }
    out
}

/// The gauges the Activity Monitor's Waits tab shows for Redis:
/// `(wait_type, count, description)` from `INFO`.
pub fn wait_rows(info: &BTreeMap<String, String>) -> Vec<(String, i64, String)> {
    const GAUGES: &[(&str, &str)] = &[
        (
            "blocked_clients",
            "Clients blocked in BLPOP/BRPOP/WAIT/XREAD BLOCK",
        ),
        (
            "clients_in_timeout_table",
            "Blocked clients with a timeout pending",
        ),
        ("connected_clients", "Open client connections"),
        (
            "tracking_clients",
            "Clients with client-side caching tracking on",
        ),
        ("pubsub_clients", "Clients in pub/sub mode"),
        ("watching_clients", "Clients with WATCHed keys"),
        ("total_watched_keys", "Keys under WATCH"),
        (
            "rejected_connections",
            "Connections refused because of maxclients (cumulative)",
        ),
        (
            "evicted_clients",
            "Clients evicted for exceeding maxmemory-clients (cumulative)",
        ),
        ("evicted_keys", "Keys evicted under maxmemory (cumulative)"),
        ("expired_keys", "Keys expired (cumulative)"),
        ("keyspace_misses", "Lookups of missing keys (cumulative)"),
        ("keyspace_hits", "Lookups of existing keys (cumulative)"),
        (
            "total_commands_processed",
            "Commands processed (cumulative)",
        ),
        ("total_forks", "Forks for RDB/AOF (cumulative)"),
        ("rdb_bgsave_in_progress", "Background RDB save running"),
        ("aof_rewrite_in_progress", "AOF rewrite running"),
        ("active_defrag_running", "Active defragmentation running"),
        ("current_fork_perc", "Progress of the running fork (%)"),
        ("latest_fork_usec", "Duration of the last fork (µs)"),
        ("instantaneous_ops_per_sec", "Commands per second (sampled)"),
        ("mem_fragmentation_ratio", "RSS / allocated memory"),
    ];
    GAUGES
        .iter()
        .filter_map(|(key, description)| {
            let value = info.get(*key)?.trim().parse::<f64>().ok()?;
            Some((
                key.to_string(),
                value.round() as i64,
                description.to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "# Server\r\nredis_version:7.2.4\r\nuptime_in_seconds:120\r\n\r\n# Clients\r\nconnected_clients:4\r\nblocked_clients:1\r\nmaxclients:10000\r\n\r\n# Memory\r\nused_memory:1048576\r\nused_memory_rss:2097152\r\nmaxmemory:0\r\ntotal_system_memory:4194304\r\n\r\n# Stats\r\ntotal_commands_processed:50\r\nkeyspace_hits:3\r\nkeyspace_misses:1\r\n\r\n# Replication\r\nrole:master\r\nconnected_slaves:0\r\n\r\n# Keyspace\r\ndb0:keys=10,expires=2,avg_ttl=500\r\ndb3:keys=5,expires=0,avg_ttl=0\r\n";

    #[test]
    fn parses_info_sections_and_keyspace() {
        let entries = parse_info(INFO);
        assert_eq!(
            entries[0],
            InfoEntry {
                section: "Server".into(),
                key: "redis_version".into(),
                value: "7.2.4".into()
            }
        );
        assert!(
            entries
                .iter()
                .any(|e| e.section == "Keyspace" && e.key == "db3")
        );
        let keyspace = parse_keyspace(INFO);
        assert_eq!(keyspace.len(), 2);
        assert_eq!(
            keyspace[0],
            KeyspaceLine {
                db: 0,
                keys: 10,
                expires: 2,
                avg_ttl_ms: 500
            }
        );
        assert_eq!(keyspace[1].db, 3);
        assert_eq!(
            info_map(INFO).get("maxclients").map(String::as_str),
            Some("10000")
        );
    }

    #[test]
    fn parses_client_list_with_missing_fields() {
        let clients = parse_client_list(
            "id=3 addr=127.0.0.1:1 name= flags=N cmd=client|list user=default\nid=9 addr=10.0.0.1:2 cmd=get\n",
        );
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].field("id"), Some("3"));
        assert_eq!(clients[0].field("name"), Some(""));
        assert_eq!(clients[1].field("user"), None);
        assert_eq!(clients[1].field("cmd"), Some("get"));
    }

    #[test]
    fn names_relatives_ahead_of_the_compat_level() {
        let mut info = BTreeMap::new();
        info.insert("redis_version".to_string(), "7.2.4".to_string());
        assert_eq!(product_and_version(&info), ("Redis".into(), "7.2.4".into()));
        info.insert("valkey_version".to_string(), "9.1.1".to_string());
        info.insert("server_name".to_string(), "valkey".to_string());
        assert_eq!(
            product_and_version(&info),
            ("Valkey".into(), "9.1.1 (Redis 7.2.4)".into())
        );
        let mut garnet = BTreeMap::new();
        garnet.insert("server_name".to_string(), "garnet".to_string());
        garnet.insert("redis_version".to_string(), "7.4.3".to_string());
        assert_eq!(
            product_and_version(&garnet),
            ("Garnet".into(), "7.4.3".into())
        );
    }

    #[test]
    fn dashboard_metrics_cover_every_group() {
        let info = info_map(INFO);
        let get = |group: &str, metric: &str| {
            dashboard_metrics(group, &info)
                .into_iter()
                .find(|(m, _)| m == metric)
                .map(|(_, v)| v)
        };
        assert_eq!(get("server", "uptime_seconds").as_deref(), Some("120"));
        assert_eq!(get("clients", "connections_active").as_deref(), Some("4"));
        assert_eq!(get("clients", "connections_max").as_deref(), Some("10000"));
        assert_eq!(get("clients", "sessions_running").as_deref(), Some("3"));
        assert_eq!(get("memory", "memory_used_mb").as_deref(), Some("1"));
        assert_eq!(get("memory", "memory_configured_mb").as_deref(), Some("4"));
        assert_eq!(get("memory", "reclaimable_mb").as_deref(), Some("1"));
        assert_eq!(get("stats", "queries_total").as_deref(), Some("50"));
        assert_eq!(get("stats", "cache_hit_ratio").as_deref(), Some("75"));
        assert_eq!(
            get("replication", "replication_lag_seconds").as_deref(),
            Some("0")
        );
        assert_eq!(get("keyspace", "objects_total").as_deref(), Some("15"));
        assert_eq!(get("keyspace", "indexes_total").as_deref(), Some("2"));
        for group in DASHBOARD_GROUPS {
            assert!(!dashboard_metrics(group, &info).is_empty(), "{group}");
        }
        assert!(dashboard_metrics("nope", &info).is_empty());
    }

    #[test]
    fn wait_rows_only_list_present_gauges() {
        let info = info_map(INFO);
        let rows = wait_rows(&info);
        assert!(
            rows.iter()
                .any(|(k, v, _)| k == "blocked_clients" && *v == 1)
        );
        assert!(!rows.iter().any(|(k, _, _)| k == "pubsub_clients"));
    }
}
