use crate::cloud_auth::{CloudAuthError, CloudToken};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use std::time::{Duration, SystemTime};

pub const RDS_TOKEN_TTL: Duration = Duration::from_secs(900);

/// `https://{host}:{port}/?Action=connect&DBUser={user}` — the exact string that is signed.
pub fn connect_url(host: &str, port: u16, username: &str) -> String {
    format!(
        "https://{host}:{port}/?Action=connect&DBUser={username}",
        host = host.trim(),
        port = port,
        username = username.trim()
    )
}

/// Which AWS in-memory service an IAM token is signed for. ElastiCache and
/// MemoryDB share the token shape but sign under different SigV4 service
/// names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AwsCacheService {
    ElastiCache,
    MemoryDb,
}

impl AwsCacheService {
    /// The SigV4 service name the token is signed for.
    pub fn signing_name(self) -> &'static str {
        match self {
            AwsCacheService::ElastiCache => "elasticache",
            AwsCacheService::MemoryDb => "memorydb",
        }
    }
}

/// What an ElastiCache / MemoryDB IAM token is signed against: the cache
/// (replication group, serverless cache, or MemoryDB cluster) *name* — not
/// the endpoint host — plus the service and, for ElastiCache Serverless, the
/// `ResourceType=ServerlessCache` marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheIamTarget {
    pub service: AwsCacheService,
    pub cache_name: String,
    pub serverless: bool,
}

/// Work out the [`CacheIamTarget`] from an ElastiCache / MemoryDB endpoint
/// host. Understood shapes (the region label may be a short code such as
/// `use1`):
///
/// * `clustercfg.{name}.{id}.{region}.cache.amazonaws.com` (also `master.`,
///   `replica.`)
/// * `{name}-ro.{id}.ng.0001.{region}.cache.amazonaws.com` (reader endpoint)
/// * `{name}-0001-001.{id}.0001.{region}.cache.amazonaws.com` (node)
/// * `{name}-{id}.serverless.{region}.cache.amazonaws.com` (Serverless)
/// * `clustercfg.{name}.{id}.memorydb.{region}.amazonaws.com` (MemoryDB)
///
/// `memorydb_hint` picks MemoryDB when the profile says so but the host
/// does not. Any other host (an IP, a private DNS alias) is refused with an
/// error that says to connect through the AWS endpoint, because a token
/// signed for the wrong name is rejected by the server as a bare
/// `WRONGPASS`.
pub fn cache_iam_target(host: &str, memorydb_hint: bool) -> Result<CacheIamTarget, CloudAuthError> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let unrecognized = || {
        CloudAuthError::AwsCacheEndpoint(format!(
            "cannot tell the ElastiCache / MemoryDB cache name from host `{host}`; \
             connect with the cluster's AWS endpoint (for example its configuration or \
             primary endpoint) so the IAM token can be signed for the right cache"
        ))
    };
    let is_memorydb_host = labels.contains(&"memorydb");
    let is_elasticache_host = labels.windows(2).any(|pair| pair == ["cache", "amazonaws"]);
    if !is_memorydb_host && !is_elasticache_host {
        return Err(unrecognized());
    }
    let service = if is_memorydb_host || (memorydb_hint && !is_elasticache_host) {
        AwsCacheService::MemoryDb
    } else {
        AwsCacheService::ElastiCache
    };
    let first = labels.first().copied().unwrap_or_default();
    let serverless =
        service == AwsCacheService::ElastiCache && labels.get(1) == Some(&"serverless");
    let name = if serverless {
        // `{name}-{id}`: the id is the last hyphen-separated part.
        first
            .rsplit_once('-')
            .filter(|(_, id)| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(|(name, _)| name)
    } else if matches!(first, "clustercfg" | "master" | "replica") {
        labels.get(1).copied()
    } else {
        Some(strip_node_suffix(first))
    };
    let name = name
        .filter(|name| is_cache_name(name))
        .ok_or_else(unrecognized)?;
    Ok(CacheIamTarget {
        service,
        cache_name: name.to_string(),
        serverless,
    })
}

/// `{name}-ro` → `{name}`; `{name}-0001-001` / `{name}-001` → `{name}`.
fn strip_node_suffix(label: &str) -> &str {
    let mut name = label.strip_suffix("-ro").unwrap_or(label);
    for width in [3, 4] {
        if let Some((head, tail)) = name.rsplit_once('-')
            && tail.len() == width
            && tail.chars().all(|c| c.is_ascii_digit())
        {
            name = head;
        }
    }
    name
}

/// ElastiCache / MemoryDB names: a letter first, then letters, digits and
/// hyphens.
fn is_cache_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `http://{cache name}/?Action=connect&User={user}` (plus
/// `&ResourceType=ServerlessCache` for Serverless) — the exact request an
/// ElastiCache / MemoryDB IAM token signs. The user id is query-encoded.
pub fn cache_connect_url(target: &CacheIamTarget, user: &str) -> Result<String, CloudAuthError> {
    let mut url = url::Url::parse(&format!("http://{}/", target.cache_name))
        .map_err(|error| CloudAuthError::AwsSigning(error.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("Action", "connect");
        query.append_pair("User", user.trim());
        if target.serverless {
            query.append_pair("ResourceType", "ServerlessCache");
        }
    }
    Ok(url.to_string())
}

/// Sign a pre-signed-URL token with explicit credentials/time (pure;
/// unit-testable with fixed inputs). `service` is the SigV4 service name
/// (`rds-db` for RDS/Aurora, `elasticache` for ElastiCache/MemoryDB).
fn sign_token(
    url: &str,
    service: &str,
    region: &str,
    identity: &Credentials,
    now: SystemTime,
) -> Result<String, CloudAuthError> {
    let mut signing_settings = SigningSettings::default();
    signing_settings.expires_in = Some(RDS_TOKEN_TTL);
    signing_settings.signature_location = aws_sigv4::http_request::SignatureLocation::QueryParams;

    let identity = identity.clone().into();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(now)
        .settings(signing_settings)
        .build()
        .map_err(|error| CloudAuthError::AwsSigning(error.to_string()))?;

    let signable_request =
        SignableRequest::new("GET", url, std::iter::empty(), SignableBody::Bytes(&[]))
            .map_err(|error| CloudAuthError::AwsSigning(error.to_string()))?;

    let (signing_instructions, _signature) = sign(signable_request, &signing_params.into())
        .map_err(|error| CloudAuthError::AwsSigning(error.to_string()))?
        .into_parts();

    let mut signed_url =
        url::Url::parse(url).map_err(|error| CloudAuthError::AwsSigning(error.to_string()))?;
    for (name, value) in signing_instructions.params() {
        signed_url.query_pairs_mut().append_pair(name, value);
    }

    // The token is the signed URL without its scheme (`https://` for RDS,
    // `http://` for ElastiCache / MemoryDB).
    let full = signed_url.to_string();
    full.strip_prefix("https://")
        .or_else(|| full.strip_prefix("http://"))
        .map(str::to_string)
        .ok_or_else(|| CloudAuthError::AwsSigning("signed URL missing http(s) prefix".into()))
}

/// Sign an RDS/Aurora IAM token with explicit credentials/time (pure).
pub fn sign_rds_token(
    host: &str,
    port: u16,
    username: &str,
    region: &str,
    identity: &Credentials,
    now: SystemTime,
) -> Result<String, CloudAuthError> {
    sign_token(
        &connect_url(host, port, username),
        "rds-db",
        region,
        identity,
        now,
    )
}

/// Sign an ElastiCache / MemoryDB IAM token with explicit credentials/time
/// (pure). The token is sent as the Redis AUTH password for the IAM user.
pub fn sign_cache_token(
    target: &CacheIamTarget,
    user: &str,
    region: &str,
    identity: &Credentials,
    now: SystemTime,
) -> Result<String, CloudAuthError> {
    sign_token(
        &cache_connect_url(target, user)?,
        target.service.signing_name(),
        region,
        identity,
        now,
    )
}

/// Load credentials (named profile or default chain). Errors name the profile.
async fn load_credentials(
    region: &str,
    profile: Option<&str>,
) -> Result<Credentials, CloudAuthError> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_types::region::Region::new(region.to_string()));
    if let Some(profile_name) = profile.filter(|name| !name.is_empty()) {
        loader = loader.profile_name(profile_name);
    }
    let config = loader.load().await;
    let provider = config
        .credentials_provider()
        .ok_or_else(|| CloudAuthError::AwsNoCredentials(String::new()))?;
    provider
        .provide_credentials()
        .await
        .map_err(|error| CloudAuthError::AwsNoCredentials(format!(" ({error})")))
}

/// Load credentials (named profile or default chain) and sign. Errors name the profile.
pub async fn mint_rds_token(
    host: &str,
    port: u16,
    username: &str,
    region: &str,
    profile: Option<&str>,
) -> Result<CloudToken, CloudAuthError> {
    let credentials = load_credentials(region, profile).await?;
    let now = SystemTime::now();
    let value = sign_rds_token(host, port, username, region, &credentials, now)?;
    Ok(CloudToken {
        value,
        expires_at: now + RDS_TOKEN_TTL,
    })
}

/// Load credentials (named profile or default chain) and sign an ElastiCache /
/// MemoryDB IAM token for the cache behind `host`. Errors name the profile.
pub async fn mint_cache_token(
    host: &str,
    memorydb_hint: bool,
    user: &str,
    region: &str,
    profile: Option<&str>,
) -> Result<CloudToken, CloudAuthError> {
    // Resolve the cache name before touching credentials, so an unusable
    // host fails fast with the actionable message.
    let target = cache_iam_target(host, memorydb_hint)?;
    let credentials = load_credentials(region, profile).await?;
    let now = SystemTime::now();
    let value = sign_cache_token(&target, user, region, &credentials, now)?;
    Ok(CloudToken {
        value,
        expires_at: now + RDS_TOKEN_TTL,
    })
}

/// Names of profiles in `~/.aws/config` + `~/.aws/credentials` (for the picker). Empty on error.
pub async fn list_profiles() -> Vec<String> {
    let fs = aws_types::os_shim_internal::Fs::real();
    let env = aws_types::os_shim_internal::Env::real();
    let files = aws_runtime::env_config::file::EnvConfigFiles::builder().build();
    aws_config::profile::load(&fs, &env, &files, None)
        .await
        .map(|profiles| {
            let mut names: Vec<String> = profiles.profiles().map(str::to_string).collect();
            names.sort();
            names
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::Credentials;

    #[test]
    fn connect_url_shape() {
        let url = connect_url("db.example.com", 5432, "jane");
        assert_eq!(
            url,
            "https://db.example.com:5432/?Action=connect&DBUser=jane"
        );
    }

    #[test]
    fn rds_token_has_expected_query_params() {
        let identity = Credentials::new("AKIATESTKEY", "secret", None, None, "test");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let token = sign_rds_token(
            "mydb.abc123.us-west-2.rds.amazonaws.com",
            5432,
            "jane",
            "us-west-2",
            &identity,
            now,
        )
        .expect("sign");
        assert!(token.starts_with(
            "mydb.abc123.us-west-2.rds.amazonaws.com:5432/?Action=connect&DBUser=jane"
        ));
        assert!(token.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(token.contains("X-Amz-Expires=900"));
        assert!(token.contains("X-Amz-Credential=AKIATESTKEY%2F"));
        assert!(token.contains("%2Fus-west-2%2Frds-db%2Faws4_request"));
        assert!(token.contains("X-Amz-Signature="));
        assert!(!token.starts_with("https://"));
    }

    #[test]
    fn rds_token_is_deterministic_for_fixed_time() {
        let identity = Credentials::new("AKIATESTKEY", "secret", None, None, "test");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let a = sign_rds_token("host", 3306, "jane", "us-west-2", &identity, now).unwrap();
        let b = sign_rds_token("host", 3306, "jane", "us-west-2", &identity, now).unwrap();
        assert_eq!(a, b);
    }

    fn target(host: &str) -> CacheIamTarget {
        cache_iam_target(host, false).expect(host)
    }

    #[test]
    fn cache_iam_target_reads_the_cache_name_from_every_endpoint_shape() {
        for host in [
            "clustercfg.my-cache.abc123.use1.cache.amazonaws.com",
            "master.my-cache.abc123.use1.cache.amazonaws.com",
            "replica.my-cache.abc123.use1.cache.amazonaws.com",
            "my-cache-ro.abc123.ng.0001.use1.cache.amazonaws.com",
            "my-cache-001.abc123.0001.use1.cache.amazonaws.com",
            "my-cache-0001-001.abc123.0001.use1.cache.amazonaws.com",
            " MASTER.My-Cache.abc123.use1.cache.amazonaws.com. ",
        ] {
            let t = target(host);
            assert_eq!(t.cache_name, "my-cache", "{host}");
            assert_eq!(t.service, AwsCacheService::ElastiCache, "{host}");
            assert!(!t.serverless, "{host}");
        }
        let serverless = target("my-cache-zbrvtr.serverless.use1.cache.amazonaws.com");
        assert_eq!(serverless.cache_name, "my-cache");
        assert!(serverless.serverless);
        let memorydb = target("clustercfg.my-cluster.abc123.memorydb.us-east-1.amazonaws.com");
        assert_eq!(memorydb.cache_name, "my-cluster");
        assert_eq!(memorydb.service, AwsCacheService::MemoryDb);
        assert!(!memorydb.serverless);
    }

    #[test]
    fn cache_iam_target_refuses_hosts_it_cannot_name() {
        for host in [
            "localhost",
            "10.0.0.5",
            "redis.internal.example.com",
            "",
            "clustercfg..abc.use1.cache.amazonaws.com",
            "-bad.abc.use1.cache.amazonaws.com",
        ] {
            assert!(
                matches!(
                    cache_iam_target(host, false),
                    Err(CloudAuthError::AwsCacheEndpoint(_))
                ),
                "{host:?}"
            );
        }
        // A MemoryDB profile on a private alias is still refused (no name).
        assert!(cache_iam_target("redis.internal.example.com", true).is_err());
    }

    #[test]
    fn cache_connect_url_signs_the_cache_name_not_the_host() {
        let t = target("clustercfg.my-cache.abc123.use1.cache.amazonaws.com");
        assert_eq!(
            cache_connect_url(&t, "iam-user").unwrap(),
            "http://my-cache/?Action=connect&User=iam-user"
        );
        let s = target("my-cache-zbrvtr.serverless.use1.cache.amazonaws.com");
        assert_eq!(
            cache_connect_url(&s, " iam-user ").unwrap(),
            "http://my-cache/?Action=connect&User=iam-user&ResourceType=ServerlessCache"
        );
        // A user id with query metacharacters is encoded, not spliced in.
        let url = cache_connect_url(&t, "a&b=c").unwrap();
        assert!(url.ends_with("User=a%26b%3Dc"), "{url}");
    }

    #[test]
    fn cache_token_signs_against_the_right_service() {
        let identity = Credentials::new("AKIATESTKEY", "secret", None, None, "test");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let t = target("clustercfg.my-cache.abc123.use1.cache.amazonaws.com");
        let token = sign_cache_token(&t, "iam-user", "us-east-1", &identity, now).expect("sign");
        assert!(token.starts_with("my-cache/?Action=connect&User=iam-user&"));
        assert!(token.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(token.contains("X-Amz-Expires=900"));
        assert!(token.contains("%2Fus-east-1%2Felasticache%2Faws4_request"));
        assert!(token.contains("X-Amz-Signature="));
        assert!(!token.starts_with("http"));
        let m = target("clustercfg.my-cluster.abc123.memorydb.us-east-1.amazonaws.com");
        let token = sign_cache_token(&m, "iam-user", "us-east-1", &identity, now).expect("sign");
        assert!(token.starts_with("my-cluster/?Action=connect&User=iam-user&"));
        assert!(token.contains("%2Fus-east-1%2Fmemorydb%2Faws4_request"));
        // Deterministic for a fixed time.
        assert_eq!(
            sign_cache_token(&t, "iam-user", "us-east-1", &identity, now).unwrap(),
            sign_cache_token(&t, "iam-user", "us-east-1", &identity, now).unwrap()
        );
    }

    #[tokio::test]
    async fn token_for_profile_none_for_sql_login() {
        let profile = sqlly_core::ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        let token = crate::cloud_auth::token_for_profile(&profile)
            .await
            .unwrap();
        assert!(token.is_none());
    }
}
