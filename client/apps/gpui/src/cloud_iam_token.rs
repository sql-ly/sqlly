//! Mints AWS RDS and Google Cloud SQL IAM database tokens for connect attempts.
//! Tokens are stamped onto `ConnectionProfile.runtime_cloud_iam_token` (in-memory
//! only, never persisted) before any engine call.

use sqlly_core::{AuthenticationType, ConnectionProfile, DatabaseType};
use sqlly_services::cloud_auth::CloudToken;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

const AWS_REFRESH_SKEW: Duration = Duration::from_secs(60);
const GCP_REFRESH_SKEW: Duration = Duration::from_secs(5 * 60);

type TokenCache = Rc<RefCell<HashMap<String, CloudToken>>>;

#[derive(Clone)]
pub struct CloudIamTokenProvider {
    cache: TokenCache,
}

impl CloudIamTokenProvider {
    pub fn new() -> Self {
        Self {
            cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn cache_key(profile: &ConnectionProfile) -> String {
        let auth = match &profile.authentication {
            AuthenticationType::AwsRdsIam { username, .. } => format!("aws-rds-iam:{username}"),
            AuthenticationType::GcpCloudSqlIam { username } => {
                format!("gcp-cloud-sql-iam:{username}")
            }
            _ => "non-iam".to_string(),
        };
        format!("{}|{}|{}", auth, profile.server, profile.port)
    }

    pub fn cached(&self, key: &str, auth: &AuthenticationType) -> Option<String> {
        let skew = match auth {
            AuthenticationType::AwsRdsIam { .. } => AWS_REFRESH_SKEW,
            AuthenticationType::GcpCloudSqlIam { .. } => GCP_REFRESH_SKEW,
            _ => return None,
        };
        self.cache
            .borrow()
            .get(key)
            .filter(|token| token.is_fresh(skew))
            .map(|token| token.value.clone())
    }

    pub async fn ensure(&self, profile: &ConnectionProfile) -> Result<String, String> {
        let key = Self::cache_key(profile);
        if let Some(token) = self.cached(&key, &profile.authentication) {
            return Ok(token);
        }
        let minted = sqlly_services::cloud_auth::token_for_profile(profile)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                crate::localization::text("connections.cloud_iam.profile_not_configured")
            })?;
        self.cache.borrow_mut().insert(key, minted.clone());
        Ok(minted.value)
    }
}

impl Default for CloudIamTokenProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Label for the default AWS credential chain in profile pickers.
pub fn aws_profile_default_label() -> String {
    crate::localization::text("connections.cloud_iam.default_aws_profile")
}

/// Infer an AWS region from a standard RDS, ElastiCache, or MemoryDB endpoint
/// host name. RDS (`….us-west-2.rds.amazonaws.com`) and MemoryDB
/// (`….memorydb.us-east-1.amazonaws.com`) spell the region out; ElastiCache
/// usually abbreviates it (`….use1.cache.amazonaws.com`), which is not
/// guessed at — the user types the region instead.
pub fn aws_region_from_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let region = if let Some(before) = host.strip_suffix(".amazonaws.com")
        && let Some((head, region)) = before.rsplit_once('.')
        && head.ends_with(".memorydb")
    {
        region
    } else {
        let before_suffix = [".rds.amazonaws.com", ".cache.amazonaws.com"]
            .iter()
            .find_map(|marker| host.strip_suffix(marker))?;
        before_suffix.rsplit('.').next()?
    };
    if region.contains('-')
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        Some(region.to_string())
    } else {
        None
    }
}

/// Row of the AWS profile select to show for a stored profile. `options` is
/// the select's items: row 0 is the localized "default" entry, the rest are
/// profile names. `None` when the stored profile is not (yet) listed.
pub fn aws_profile_select_row(stored: &str, options: &[String]) -> Option<usize> {
    let stored = stored.trim();
    if stored.is_empty() {
        return Some(0);
    }
    options
        .iter()
        .skip(1)
        .position(|name| name == stored)
        .map(|ix| ix + 1)
}

/// Profile stored for the select's confirmed `row`: row 0 is the default
/// entry (stored empty), so a profile literally named like that label still
/// round-trips as itself.
pub fn aws_profile_from_row(row: usize, options: &[String]) -> String {
    if row == 0 {
        String::new()
    } else {
        options.get(row).cloned().unwrap_or_default()
    }
}

pub fn gcp_iam_username_placeholder(engine: DatabaseType) -> &'static str {
    match engine {
        DatabaseType::MariaDb | DatabaseType::MySql => "alice",
        _ => "alice@example.com",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn aws_profile_select_maps_rows_not_label_text() {
        let default = super::aws_profile_default_label();
        let options = vec![default.clone(), "prod".to_string(), default.clone()];
        assert_eq!(super::aws_profile_from_row(0, &options), "");
        assert_eq!(super::aws_profile_from_row(1, &options), "prod");
        // A profile named exactly like the default entry is still itself.
        assert_eq!(super::aws_profile_from_row(2, &options), default);
        assert_eq!(super::aws_profile_from_row(9, &options), "");
        assert_eq!(super::aws_profile_select_row("", &options), Some(0));
        assert_eq!(super::aws_profile_select_row(" prod ", &options), Some(1));
        assert_eq!(super::aws_profile_select_row(&default, &options), Some(2));
        assert_eq!(super::aws_profile_select_row("missing", &options), None);
    }

    use super::*;
    use sqlly_core::DatabaseType;

    #[test]
    fn cache_returns_fresh_only() {
        let provider = CloudIamTokenProvider::new();
        let key = "aws-rds-iam:jane|db.example.com|5432";
        let auth = AuthenticationType::AwsRdsIam {
            username: "jane".into(),
            region: "us-west-2".into(),
            profile: None,
        };
        provider.cache.borrow_mut().insert(
            key.to_string(),
            CloudToken {
                value: "tok".into(),
                expires_at: std::time::SystemTime::now() + Duration::from_secs(600),
            },
        );
        assert_eq!(provider.cached(key, &auth).as_deref(), Some("tok"));
        provider.cache.borrow_mut().insert(
            key.to_string(),
            CloudToken {
                value: "stale".into(),
                expires_at: std::time::SystemTime::now() + Duration::from_secs(30),
            },
        );
        assert!(provider.cached(key, &auth).is_none());
    }

    #[test]
    fn cache_key_distinguishes_profiles() {
        let a = ConnectionProfile::aws_rds_iam(
            "a",
            "db-a.example.com",
            5432,
            DatabaseType::Postgres,
            "jane",
            "us-west-2",
        );
        let b = ConnectionProfile::aws_rds_iam(
            "b",
            "db-b.example.com",
            5432,
            DatabaseType::Postgres,
            "jane",
            "us-west-2",
        );
        assert_ne!(
            CloudIamTokenProvider::cache_key(&a),
            CloudIamTokenProvider::cache_key(&b)
        );
    }

    #[test]
    fn aws_region_from_host_matches_rds_endpoint() {
        assert_eq!(
            aws_region_from_host("mydb.abc123.eu-central-1.rds.amazonaws.com").as_deref(),
            Some("eu-central-1")
        );
        assert_eq!(aws_region_from_host("localhost"), None);
    }

    #[test]
    fn aws_region_from_host_matches_elasticache_and_memorydb_endpoints() {
        assert_eq!(
            aws_region_from_host("clustercfg.my-cluster.abc123.memorydb.eu-west-1.amazonaws.com")
                .as_deref(),
            Some("eu-west-1")
        );
        // A spelled-out ElastiCache region is read; the usual short code
        // (`use1`) is not guessed at.
        assert_eq!(
            aws_region_from_host("clustercfg.xxxx.us-east-1.cache.amazonaws.com").as_deref(),
            Some("us-east-1")
        );
        assert_eq!(
            aws_region_from_host("clustercfg.my-cache.abc123.use1.cache.amazonaws.com"),
            None
        );
        assert_eq!(aws_region_from_host("redis.memorydb.example.com"), None);
    }
}
