//! The host an RDS IAM token is signed for. RDS checks the token against its
//! own endpoint name (`*.rds.amazonaws.com`), so a connection typed with a
//! company DNS alias (`db.example.com` CNAME → the RDS endpoint) must sign
//! for the endpoint behind the alias. The TCP connection still goes to the
//! host the user typed; only the signature changes.
//!
//! The chain is followed one CNAME at a time and stops at the first RDS
//! endpoint, because the endpoint itself is usually a CNAME to an internal
//! EC2 host name that RDS would reject.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Longest alias chain followed before giving up (and signing the typed host).
const MAX_CNAME_HOPS: usize = 8;

/// Total time allowed for following the chain; past it the typed host is used.
const RESOLVE_BUDGET: Duration = Duration::from_secs(3);

pub type CnameFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

/// One CNAME hop: the alias target of `host`, or `None` when `host` has no
/// CNAME record (or the lookup failed).
pub trait CnameResolver: Send + Sync {
    fn cname<'a>(&'a self, host: &'a str) -> CnameFuture<'a>;
}

/// True for an RDS / Aurora / RDS Proxy endpoint in any partition.
pub fn is_rds_endpoint(host: &str) -> bool {
    let host = normalize(host);
    host.ends_with(".rds.amazonaws.com") || host.ends_with(".rds.amazonaws.com.cn")
}

fn normalize(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// The host to sign an RDS IAM token for: `typed` itself when it already is
/// an RDS endpoint, else the first RDS endpoint on its CNAME chain, else
/// `typed` unchanged (no alias, lookup failure, timeout, or the chain never
/// reaches RDS).
pub async fn rds_signing_host(typed: &str, resolver: &dyn CnameResolver) -> String {
    rds_signing_host_within(typed, resolver, RESOLVE_BUDGET).await
}

async fn rds_signing_host_within(
    typed: &str,
    resolver: &dyn CnameResolver,
    budget: Duration,
) -> String {
    let typed_trimmed = typed.trim();
    if typed_trimmed.is_empty() || is_rds_endpoint(typed_trimmed) {
        return typed_trimmed.to_string();
    }
    if typed_trimmed.parse::<std::net::IpAddr>().is_ok() {
        return typed_trimmed.to_string();
    }
    let follow = async {
        let mut current = normalize(typed_trimmed);
        let mut seen = vec![current.clone()];
        for _ in 0..MAX_CNAME_HOPS {
            let next = normalize(&resolver.cname(&current).await?);
            if next.is_empty() || seen.contains(&next) {
                return None;
            }
            if is_rds_endpoint(&next) {
                return Some(next);
            }
            seen.push(next.clone());
            current = next;
        }
        None
    };
    match tokio::time::timeout(budget, follow).await {
        Ok(Some(endpoint)) => {
            tracing::info!(
                typed = typed_trimmed,
                endpoint = %endpoint,
                "signing RDS IAM token for the endpoint behind a CNAME"
            );
            endpoint
        }
        _ => typed_trimmed.to_string(),
    }
}

/// CNAME lookups through the system's configured DNS servers.
pub struct SystemCnameResolver;

impl CnameResolver for SystemCnameResolver {
    fn cname<'a>(&'a self, host: &'a str) -> CnameFuture<'a> {
        Box::pin(async move {
            use hickory_resolver::proto::rr::{RData, RecordType};
            let resolver = hickory_resolver::TokioResolver::builder_tokio()
                .ok()?
                .build()
                .ok()?;
            // A trailing dot keeps the system search domains out of it.
            let fqdn = format!("{}.", normalize(host));
            let lookup = resolver
                .lookup(fqdn.as_str(), RecordType::CNAME)
                .await
                .ok()?;
            let owner = normalize(host);
            let mut first = None;
            for record in lookup.answers() {
                if let RData::CNAME(target) = &record.data {
                    let target = target.0.to_ascii();
                    if normalize(&record.name.to_ascii()) == owner {
                        return Some(target);
                    }
                    first.get_or_insert(target);
                }
            }
            first
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeResolver {
        records: HashMap<String, String>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeResolver {
        fn new(records: &[(&str, &str)]) -> Self {
            Self {
                records: records
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                asked: Mutex::new(Vec::new()),
            }
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl CnameResolver for FakeResolver {
        fn cname<'a>(&'a self, host: &'a str) -> CnameFuture<'a> {
            self.asked.lock().unwrap().push(host.to_string());
            let answer = self.records.get(host).cloned();
            Box::pin(async move { answer })
        }
    }

    const ENDPOINT: &str = "mydb.abc123.us-east-1.rds.amazonaws.com";

    #[tokio::test]
    async fn an_rds_endpoint_is_signed_as_typed_without_a_lookup() {
        let resolver = FakeResolver::new(&[]);
        assert_eq!(rds_signing_host(ENDPOINT, &resolver).await, ENDPOINT);
        assert!(resolver.asked().is_empty());
    }

    #[tokio::test]
    async fn a_cname_alias_signs_for_the_rds_endpoint_behind_it() {
        let resolver =
            FakeResolver::new(&[("db.example.com", "mydb.abc123.us-east-1.rds.amazonaws.com.")]);
        assert_eq!(
            rds_signing_host("db.example.com", &resolver).await,
            ENDPOINT
        );
    }

    #[tokio::test]
    async fn a_multi_hop_chain_is_followed_case_insensitively() {
        let resolver = FakeResolver::new(&[
            ("db.example.com", "DB.Internal.Example.NET."),
            (
                "db.internal.example.net",
                "MyDB.abc123.us-east-1.RDS.amazonaws.com",
            ),
        ]);
        assert_eq!(
            rds_signing_host("DB.example.com.", &resolver).await,
            ENDPOINT
        );
    }

    #[tokio::test]
    async fn the_chain_stops_at_the_first_rds_endpoint_not_the_ec2_host_behind_it() {
        let resolver = FakeResolver::new(&[
            ("db.example.com", ENDPOINT),
            (ENDPOINT, "ec2-3-4-5-6.compute-1.amazonaws.com"),
        ]);
        assert_eq!(
            rds_signing_host("db.example.com", &resolver).await,
            ENDPOINT
        );
        assert_eq!(resolver.asked(), vec!["db.example.com".to_string()]);
    }

    #[tokio::test]
    async fn rds_proxy_and_china_endpoints_count_as_rds() {
        assert!(is_rds_endpoint(
            "proxy-1.proxy-abc123.us-east-1.rds.amazonaws.com"
        ));
        assert!(is_rds_endpoint(
            "mydb.abc123.rds.cn-north-1.rds.amazonaws.com.cn"
        ));
        assert!(!is_rds_endpoint("db.example.com"));
        assert!(!is_rds_endpoint("rds.amazonaws.com.evil.example"));
    }

    #[tokio::test]
    async fn no_alias_or_a_failed_lookup_keeps_the_typed_host() {
        let resolver = FakeResolver::new(&[]);
        assert_eq!(
            rds_signing_host("db.example.com", &resolver).await,
            "db.example.com"
        );
    }

    #[tokio::test]
    async fn a_chain_that_never_reaches_rds_keeps_the_typed_host() {
        let resolver = FakeResolver::new(&[("db.example.com", "lb.example.net")]);
        assert_eq!(
            rds_signing_host("db.example.com", &resolver).await,
            "db.example.com"
        );
    }

    #[tokio::test]
    async fn a_cname_loop_keeps_the_typed_host() {
        let resolver = FakeResolver::new(&[
            ("a.example.com", "b.example.com"),
            ("b.example.com", "a.example.com"),
        ]);
        assert_eq!(
            rds_signing_host("a.example.com", &resolver).await,
            "a.example.com"
        );
    }

    #[tokio::test]
    async fn ip_addresses_are_never_looked_up() {
        let resolver = FakeResolver::new(&[]);
        assert_eq!(rds_signing_host("10.0.0.5", &resolver).await, "10.0.0.5");
        assert!(resolver.asked().is_empty());
    }

    #[tokio::test]
    async fn a_resolver_that_hangs_falls_back_to_the_typed_host() {
        struct Hangs;
        impl CnameResolver for Hangs {
            fn cname<'a>(&'a self, _host: &'a str) -> CnameFuture<'a> {
                Box::pin(std::future::pending())
            }
        }
        let host =
            rds_signing_host_within("db.example.com", &Hangs, Duration::from_millis(50)).await;
        assert_eq!(host, "db.example.com");
    }

    /// Needs network: `cargo test -p sqlly-services system_resolver -- --ignored`.
    #[tokio::test]
    #[ignore = "needs DNS"]
    async fn system_resolver_reads_a_real_cname() {
        let target = SystemCnameResolver.cname("www.github.com").await;
        assert_eq!(
            target.as_deref().map(normalize).as_deref(),
            Some("github.com")
        );
        assert_eq!(SystemCnameResolver.cname("github.com").await, None);
    }
}
