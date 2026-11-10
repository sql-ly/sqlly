//! Auto-detecting which Redis-compatible engine a profile talks to.
//!
//! A profile saved with [`ConnectionProfile::engine_auto_detect`] keeps its
//! `database_type` as the flavor detected so far. Two passes fill it in:
//!
//! 1. [`from_host`] while the profile is edited: AWS endpoint names identify
//!    ElastiCache and MemoryDB, which decide TLS and IAM sign-in before any
//!    connection is made.
//! 2. [`detected`] after connecting, from the product `INFO server` announces
//!    (Valkey, KeyDB, DragonflyDB, or plain Redis).
//!
//! A managed-service host wins over the product name: an ElastiCache cache
//! running Valkey still needs ElastiCache's IAM signing and ACL rules.
//! TLS is never probed by trial — falling back to plaintext when a TLS
//! handshake fails would silently drop encryption.
//!
//! [`ConnectionProfile::engine_auto_detect`]: crate::ConnectionProfile::engine_auto_detect

use crate::DatabaseType;

/// The managed Redis service an endpoint host names, or `None` for any
/// other host (an IP, a private DNS alias, `localhost`). The server field
/// may list several seed hosts (`h1:7000, h2:7001`); the first one decides.
pub fn from_host(server: &str) -> Option<DatabaseType> {
    let first = server.split(',').next()?.trim();
    let host = match first.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => first,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let on_aws = labels.contains(&"amazonaws");
    if !on_aws {
        return None;
    }
    if labels.contains(&"memorydb") {
        Some(DatabaseType::AmazonMemoryDb)
    } else if labels.windows(2).any(|pair| pair == ["cache", "amazonaws"]) {
        Some(DatabaseType::AmazonElastiCache)
    } else {
        None
    }
}

/// The engine a product name from `INFO server` stands for (the first value
/// of `sqlly_engines::redis::info::product_and_version`). Unknown products
/// (Garnet, a future fork) read as plain Redis.
pub fn from_product(product: &str) -> DatabaseType {
    match product.trim().to_ascii_lowercase().as_str() {
        "valkey" => DatabaseType::Valkey,
        "keydb" => DatabaseType::KeyDb,
        "dragonflydb" | "dragonfly" => DatabaseType::DragonflyDb,
        _ => DatabaseType::Redis,
    }
}

/// The flavor an auto-detect profile should hold: the managed service its
/// host names, else the product the server announced, else plain Redis.
pub fn detected(server: &str, product: Option<&str>) -> DatabaseType {
    from_host(server)
        .or_else(|| product.map(from_product))
        .unwrap_or(DatabaseType::Redis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_endpoints_name_their_service() {
        for host in [
            "clustercfg.sessions.abc123.use1.cache.amazonaws.com",
            "master.sessions.abc123.use1.cache.amazonaws.com:6379",
            "sessions-abc123.serverless.use1.cache.amazonaws.com",
            "sessions-ro.abc123.ng.0001.use1.cache.amazonaws.com.",
            "node.abc.cache.amazonaws.com.cn",
        ] {
            assert_eq!(
                from_host(host),
                Some(DatabaseType::AmazonElastiCache),
                "{host}"
            );
        }
        assert_eq!(
            from_host("clustercfg.orders.abc123.memorydb.us-east-1.amazonaws.com"),
            Some(DatabaseType::AmazonMemoryDb)
        );
        assert_eq!(
            from_host("CLUSTERCFG.ORDERS.ABC.MEMORYDB.US-EAST-1.AMAZONAWS.COM:6379, other:7000"),
            Some(DatabaseType::AmazonMemoryDb)
        );
    }

    #[test]
    fn other_hosts_name_no_service() {
        for host in [
            "",
            "localhost",
            "127.0.0.1:6379",
            "redis.internal.example.com",
            "mydb.abc.us-east-1.rds.amazonaws.com",
            "cache.example.com",
            "localhost:7000, clustercfg.x.y.use1.cache.amazonaws.com",
        ] {
            assert_eq!(from_host(host), None, "{host}");
        }
    }

    #[test]
    fn products_map_to_flavors() {
        assert_eq!(from_product("Valkey"), DatabaseType::Valkey);
        assert_eq!(from_product("KeyDB"), DatabaseType::KeyDb);
        assert_eq!(from_product("DragonflyDB"), DatabaseType::DragonflyDb);
        assert_eq!(from_product("Redis"), DatabaseType::Redis);
        assert_eq!(from_product("Garnet"), DatabaseType::Redis);
    }

    #[test]
    fn managed_host_wins_over_the_announced_product() {
        assert_eq!(
            detected("clustercfg.a.b.use1.cache.amazonaws.com", Some("Valkey")),
            DatabaseType::AmazonElastiCache
        );
        assert_eq!(detected("10.0.0.5", Some("Valkey")), DatabaseType::Valkey);
        assert_eq!(detected("10.0.0.5", None), DatabaseType::Redis);
    }
}
