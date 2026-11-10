#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{AuthenticationType, DatabaseType};

fn env_or_skip(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

#[tokio::test]
async fn live_rds_iam_postgres_connects() {
    let Some(host) = env_or_skip("SQLLY_RDS_IAM_HOST") else {
        eprintln!("skipping: SQLLY_RDS_IAM_HOST not set");
        return;
    };
    let Some(user) = env_or_skip("SQLLY_RDS_IAM_USER") else {
        eprintln!("skipping: SQLLY_RDS_IAM_USER not set");
        return;
    };
    let Some(region) = env_or_skip("SQLLY_RDS_IAM_REGION") else {
        eprintln!("skipping: SQLLY_RDS_IAM_REGION not set");
        return;
    };
    let port = std::env::var("SQLLY_RDS_IAM_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5432);
    let database = env_or_skip("SQLLY_RDS_IAM_DATABASE");
    let profile_name = env_or_skip("SQLLY_RDS_IAM_PROFILE");

    let mut profile = sqlly_core::ConnectionProfile::aws_rds_iam(
        "rds-iam-pg",
        &host,
        port,
        DatabaseType::Postgres,
        &user,
        &region,
    );
    profile.database = database;
    if let AuthenticationType::AwsRdsIam {
        profile: aws_profile,
        ..
    } = &mut profile.authentication
    {
        *aws_profile = profile_name;
    }
    let token = sqlly_services::cloud_auth::token_for_profile(&profile)
        .await
        .expect("mint token")
        .expect("iam token");
    profile.runtime_cloud_iam_token = Some(token.value);
    let _client = sqlly_engines::pg::open_client(&profile)
        .await
        .expect("connect");
}

#[tokio::test]
async fn live_rds_iam_mysql_connects() {
    let Some(host) = env_or_skip("SQLLY_RDS_IAM_MYSQL_HOST") else {
        eprintln!("skipping: SQLLY_RDS_IAM_MYSQL_HOST not set");
        return;
    };
    let Some(user) = env_or_skip("SQLLY_RDS_IAM_MYSQL_USER") else {
        eprintln!("skipping: SQLLY_RDS_IAM_MYSQL_USER not set");
        return;
    };
    let Some(region) = env_or_skip("SQLLY_RDS_IAM_MYSQL_REGION") else {
        eprintln!("skipping: SQLLY_RDS_IAM_MYSQL_REGION not set");
        return;
    };
    let port = std::env::var("SQLLY_RDS_IAM_MYSQL_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3306);
    let database = env_or_skip("SQLLY_RDS_IAM_MYSQL_DATABASE");
    let profile_name = env_or_skip("SQLLY_RDS_IAM_MYSQL_PROFILE");

    let mut profile = sqlly_core::ConnectionProfile::aws_rds_iam(
        "rds-iam-mysql",
        &host,
        port,
        DatabaseType::MySql,
        &user,
        &region,
    );
    profile.database = database;
    if let AuthenticationType::AwsRdsIam {
        profile: aws_profile,
        ..
    } = &mut profile.authentication
    {
        *aws_profile = profile_name;
    }
    let token = sqlly_services::cloud_auth::token_for_profile(&profile)
        .await
        .expect("mint token")
        .expect("iam token");
    profile.runtime_cloud_iam_token = Some(token.value);
    let _client = sqlly_engines::mysql::open_client(&profile)
        .await
        .expect("connect");
}

#[tokio::test]
async fn live_cloud_sql_iam_postgres_connects() {
    let Some(host) = env_or_skip("SQLLY_GCP_IAM_HOST") else {
        eprintln!("skipping: SQLLY_GCP_IAM_HOST not set");
        return;
    };
    let Some(user) = env_or_skip("SQLLY_GCP_IAM_USER") else {
        eprintln!("skipping: SQLLY_GCP_IAM_USER not set");
        return;
    };
    let database = env_or_skip("SQLLY_GCP_IAM_DATABASE");
    let port = std::env::var("SQLLY_GCP_IAM_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5432);

    let mut profile = sqlly_core::ConnectionProfile::gcp_cloud_sql_iam(
        "cloudsql-iam",
        &host,
        port,
        DatabaseType::Postgres,
        &user,
    );
    profile.database = database;
    let token = sqlly_services::cloud_auth::token_for_profile(&profile)
        .await
        .expect("mint token")
        .expect("iam token");
    profile.runtime_cloud_iam_token = Some(token.value);
    let _client = sqlly_engines::pg::open_client(&profile)
        .await
        .expect("connect");
}
