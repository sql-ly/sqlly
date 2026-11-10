//! Env-gated live test for native pg_dump integration.

use std::collections::HashMap;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use sqlly_core::DatabaseType;
use sqlly_core::connection::{ConnectionProfile, SecretProvider, SecretRef};
use sqlly_services::tool_discovery::ToolLocation;
use sqlly_services::{
    DumpOptions, NativeDumpRequest, NativeTool, PgFormat, Services, locate_all, run_native_dump,
};

fn live_enabled() -> bool {
    std::env::var("SQLLY_REQUIRE_LIVE_SQL").as_deref() == Ok("1")
}

fn pg_profile() -> Option<ConnectionProfile> {
    let host = std::env::var("SQLLY_PG_HOST").ok()?;
    let port: u16 = std::env::var("SQLLY_PG_PORT").ok()?.parse().ok()?;
    let user = std::env::var("SQLLY_PG_USER").ok()?;
    let password = std::env::var("SQLLY_PG_PASSWORD").ok()?;
    let database = std::env::var("SQLLY_PG_DATABASE").ok()?;
    let mut profile = ConnectionProfile::sql_login(
        "live-pg",
        host,
        user,
        SecretRef {
            provider: SecretProvider::InMemory,
            key: password,
        },
    );
    profile.port = port;
    profile.database = Some(database.clone());
    profile.database_type = DatabaseType::Postgres;
    profile.encrypt = false;
    Some(profile)
}

#[tokio::test]
async fn native_dump_plain_pg_live() {
    if !live_enabled() {
        return;
    }
    let Some(profile) = pg_profile() else {
        return;
    };
    let locations = locate_all(&HashMap::new()).await;
    let Some(loc) = locations.iter().find(|l| l.tool == NativeTool::PgDump) else {
        return;
    };
    let tool = ToolLocation {
        tool: loc.tool,
        path: loc.path.clone(),
        version: loc.version.clone(),
        source: loc.source,
    };
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("dump.sql");
    let database = profile
        .database
        .clone()
        .unwrap_or_else(|| "postgres".into());
    let req = NativeDumpRequest {
        database,
        output: output.clone(),
        options: DumpOptions::default(),
        tables: None,
        pg_format: PgFormat::Plain,
        pg_no_owner: true,
        mysql_single_transaction: true,
        mysql_extra_args: Vec::new(),
        mysql_strip_definer: false,
    };
    let services = Services::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let report = tokio::time::timeout(
        Duration::from_secs(120),
        run_native_dump(&services, &profile, &tool, &req, cancel, &mut |_| {}),
    )
    .await
    .expect("timed out")
    .expect("dump failed");
    assert!(!report.cancelled);
    let head = std::fs::read(&output).unwrap();
    let prefix = String::from_utf8_lossy(&head[..head.len().min(64)]);
    assert!(
        prefix.starts_with("--\n-- PostgreSQL database dump")
            || prefix.starts_with("-- PostgreSQL database dump"),
        "unexpected dump prefix: {prefix}"
    );
}
