//! Cloud IAM database tokens. Native only: the browser build never links the
//! AWS/GCP SDKs; callers use `sqlly_core::AuthenticationType::is_cloud_iam` and
//! the capability matrix to keep these paths unreachable on wasm.
#![forbid(unsafe_code)]
#![cfg(not(target_arch = "wasm32"))]

pub mod aws;
pub mod gcp;
pub mod rds_host;

use sqlly_core::{AuthenticationType, ConnectionProfile};
use std::time::{Duration, SystemTime};

#[derive(Debug, thiserror::Error)]
pub enum CloudAuthError {
    #[error("no AWS credentials found{0}; run `aws sso login` or `aws configure` and try again")]
    AwsNoCredentials(String),
    #[error("AWS signing failed: {0}")]
    AwsSigning(String),
    #[error("{0}")]
    AwsCacheEndpoint(String),
    #[error(
        "no Google Cloud credentials found; run `gcloud auth application-default login` and try again"
    )]
    GcpNoCredentials,
    #[error("Google token request failed: {0}")]
    GcpToken(String),
}

impl CloudAuthError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::AwsNoCredentials(detail) => DisplayMessage::new(
                "services.cloud_auth.errors.aws_no_credentials",
                format!("no AWS credentials found{detail}; run `aws sso login` or `aws configure` and try again"),
            )
            .with_arg("detail", detail),
            Self::AwsSigning(detail) => DisplayMessage::new(
                "services.cloud_auth.errors.aws_signing",
                format!("AWS signing failed: {detail}"),
            )
            .with_arg("detail", detail),
            Self::AwsCacheEndpoint(detail) => DisplayMessage::raw(detail),
            Self::GcpNoCredentials => DisplayMessage::new(
                "services.cloud_auth.errors.gcp_no_credentials",
                "no Google Cloud credentials found; run `gcloud auth application-default login` and try again",
            ),
            Self::GcpToken(detail) => DisplayMessage::new(
                "services.cloud_auth.errors.gcp_token",
                format!("Google token request failed: {detail}"),
            )
            .with_arg("detail", detail),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CloudToken {
    pub value: String,
    pub expires_at: SystemTime,
}

impl CloudToken {
    pub fn is_fresh(&self, skew: Duration) -> bool {
        self.expires_at
            > SystemTime::now()
                .checked_add(skew)
                .unwrap_or(SystemTime::now())
    }
}

/// Mint the token a profile needs, or `Ok(None)` for non-IAM auth.
pub async fn token_for_profile(
    profile: &ConnectionProfile,
) -> Result<Option<CloudToken>, CloudAuthError> {
    token_for_profile_with(profile, &rds_host::SystemCnameResolver).await
}

/// The host an RDS / Aurora IAM token is signed for: the RDS endpoint behind
/// the profile's server when the user typed a CNAME alias, else the server
/// as typed. The connection itself still dials the typed server.
pub async fn rds_token_host(
    profile: &ConnectionProfile,
    resolver: &dyn rds_host::CnameResolver,
) -> String {
    rds_host::rds_signing_host(&profile.server, resolver).await
}

async fn token_for_profile_with(
    profile: &ConnectionProfile,
    resolver: &dyn rds_host::CnameResolver,
) -> Result<Option<CloudToken>, CloudAuthError> {
    match &profile.authentication {
        AuthenticationType::AwsRdsIam {
            username,
            region,
            profile: aws_profile,
        } => {
            // ElastiCache / MemoryDB sign IAM tokens for the cache *name*
            // (`elasticache` / `memorydb` service, `User=`); RDS/Aurora sign
            // for host:port (`rds-db`, `DBUser=`). Both tokens authenticate
            // as the AUTH/password value.
            let minted = if uses_elasticache_iam(profile) {
                aws::mint_cache_token(
                    &profile.server,
                    profile.database_type == sqlly_core::DatabaseType::AmazonMemoryDb,
                    username,
                    region,
                    aws_profile.as_deref(),
                )
                .await?
            } else {
                let signing_host = rds_token_host(profile, resolver).await;
                aws::mint_rds_token(
                    &signing_host,
                    profile.port,
                    username,
                    region,
                    aws_profile.as_deref(),
                )
                .await?
            };
            Ok(Some(minted))
        }
        AuthenticationType::GcpCloudSqlIam { .. } => Ok(Some(gcp::mint_cloud_sql_token().await?)),
        _ => Ok(None),
    }
}

/// Redis-protocol AWS IAM profiles authenticate to ElastiCache / MemoryDB,
/// which sign a different request than RDS (see [`aws::mint_cache_token`]).
fn uses_elasticache_iam(profile: &ConnectionProfile) -> bool {
    profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;

    #[test]
    fn redis_protocol_profiles_mint_against_elasticache_everything_else_against_rds() {
        for db in [
            DatabaseType::Redis,
            DatabaseType::AmazonElastiCache,
            DatabaseType::AmazonMemoryDb,
        ] {
            let profile = ConnectionProfile::aws_rds_iam(
                "cache",
                "clustercfg.use1.cache.amazonaws.com",
                6379,
                db,
                "iam-user",
                "us-east-1",
            );
            assert!(uses_elasticache_iam(&profile), "{db:?}");
        }
        let pg = ConnectionProfile::aws_rds_iam(
            "db",
            "mydb.abc123.us-west-2.rds.amazonaws.com",
            5432,
            DatabaseType::Postgres,
            "jane",
            "us-west-2",
        );
        assert!(!uses_elasticache_iam(&pg));
    }

    struct AliasResolver;

    impl rds_host::CnameResolver for AliasResolver {
        fn cname<'a>(&'a self, host: &'a str) -> rds_host::CnameFuture<'a> {
            let answer = (host == "db.example.com")
                .then(|| "mydb.abc123.us-west-2.rds.amazonaws.com.".to_string());
            Box::pin(async move { answer })
        }
    }

    #[tokio::test]
    async fn rds_iam_signs_for_the_endpoint_behind_a_cname_but_keeps_the_typed_server() {
        let profile = ConnectionProfile::aws_rds_iam(
            "db",
            "db.example.com",
            5432,
            DatabaseType::Postgres,
            "jane",
            "us-west-2",
        );
        assert_eq!(
            rds_token_host(&profile, &AliasResolver).await,
            "mydb.abc123.us-west-2.rds.amazonaws.com"
        );
        assert_eq!(profile.server, "db.example.com");
    }

    #[tokio::test]
    async fn rds_iam_signs_an_unaliased_host_as_typed() {
        let profile = ConnectionProfile::aws_rds_iam(
            "db",
            "other.example.com",
            3306,
            DatabaseType::MySql,
            "jane",
            "us-west-2",
        );
        assert_eq!(
            rds_token_host(&profile, &AliasResolver).await,
            "other.example.com"
        );
    }
}
