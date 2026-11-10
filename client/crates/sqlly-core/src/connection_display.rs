//! Display and ordering helpers shared by the connection-management tree and
//! the unified Servers Explorer tree. Both surfaces present the same
//! connection/discovery data and must label, rank, and badge it identically.

use crate::connection::{AuthenticationType, ConnectionEnvironment};
use crate::connection_management::AzureSqlDiscovery;

pub(crate) fn environment_label(environment: ConnectionEnvironment) -> String {
    match environment {
        ConnectionEnvironment::Production => "PRODUCTION",
        ConnectionEnvironment::Staging => "STAGING",
        ConnectionEnvironment::Test => "TEST",
        ConnectionEnvironment::Dev => "DEV",
        ConnectionEnvironment::Local => "LOCAL",
        ConnectionEnvironment::Unset => "UNSET",
    }
    .to_string()
}

/// Sort rank: production first, unset last.
pub(crate) fn environment_rank(environment: ConnectionEnvironment) -> u8 {
    match environment {
        ConnectionEnvironment::Production => 0,
        ConnectionEnvironment::Staging => 1,
        ConnectionEnvironment::Test => 2,
        ConnectionEnvironment::Dev => 3,
        ConnectionEnvironment::Local => 4,
        ConnectionEnvironment::Unset => 5,
    }
}

/// Stable grouping key for an authentication method (same server + same auth
/// collapse into one tree node).
pub(crate) fn auth_key(auth: &AuthenticationType) -> String {
    match auth {
        AuthenticationType::SqlLogin { username, .. } => format!("sql:{username}"),
        AuthenticationType::Integrated => "integrated".to_string(),
        AuthenticationType::EntraDeviceCode => "entra-device-code".to_string(),
        AuthenticationType::EntraBrowser => "entra-browser".to_string(),
        AuthenticationType::AwsRdsIam { username, .. } => format!("aws-rds-iam:{username}"),
        AuthenticationType::GcpCloudSqlIam { username } => format!("gcp-cloud-sql-iam:{username}"),
        AuthenticationType::AuthToken { .. } => "auth-token".to_string(),
    }
}

/// Explicit Azure ordering spec: accounts sort by `username` then `tenant_id`
/// (so the same username across tenants stays grouped but distinct), then by
/// subscription, then servers by `server_name` then `location`, then database.
pub(crate) fn azure_sort_key(
    discovery: &AzureSqlDiscovery,
) -> (String, String, String, String, String, String) {
    (
        discovery.username.to_ascii_lowercase(),
        discovery.tenant_id.clone(),
        discovery.subscription_id.clone(),
        discovery.server_name.to_ascii_lowercase(),
        discovery.location.to_ascii_lowercase(),
        discovery.database_name.to_ascii_lowercase(),
    )
}

pub(crate) fn azure_database_badges(discovery: &AzureSqlDiscovery) -> Vec<String> {
    let mut badges = if discovery.linked_profile_id.is_some() {
        vec!["Linked".to_string()]
    } else {
        vec!["Discovered".to_string()]
    };
    if let Some(tier) = discovery.service_tier.as_ref().filter(|t| !t.is_empty()) {
        badges.push(tier.clone());
    }
    if let Some(status) = discovery.status.as_ref().filter(|s| !s.is_empty()) {
        badges.push(status.clone());
    }
    badges
}
