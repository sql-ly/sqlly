//! The Redis engine through the service layer, against a real server: the
//! connection manager's database list, the query executor running command
//! scripts and every tool's virtual command (dashboard groups, activity
//! views, disk usage, server info), the read-only policy the AI/MCP gates
//! apply, and the mutation classification the confirmation dialog uses.
//!
//! Opt-in: set `SQLLY_REDIS_URL` (for example `redis://127.0.0.1:6380/0`,
//! the docker container from `docker run -d --name sqlly-redis -p 6380:6379
//! redis:7`); every test self-skips without it.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType};
use sqlly_engines::redis::live_profile_from_env;
use sqlly_services::admin::activity::ActivityMonitor;
use sqlly_services::admin::disk_usage::{DiskDialect, DiskUsage};
use sqlly_services::query::QueryEvent;
use sqlly_services::{ActivityDialect, Dashboard, DashboardDialect, Services};

fn profile() -> Option<ConnectionProfile> {
    let mut profile = live_profile_from_env("redis-services")?;
    // Logical database 11 so a developer's own keys are untouched.
    profile.database = Some("11".into());
    profile.auto_wrap_rollback = false;
    Some(profile)
}

/// Run a script through the query executor and collect columns + rows per
/// result set (the tool dialogs' contract).
async fn run(
    services: &Services,
    profile: &ConnectionProfile,
    text: &str,
) -> Vec<(Vec<String>, Vec<Vec<CellValue>>)> {
    let mut handle = services
        .queries
        .execute(profile, text, 256)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    let mut sets: Vec<(Vec<String>, Vec<Vec<CellValue>>)> = Vec::new();
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::ResultSet { index, columns } => {
                if sets.len() <= index {
                    sets.resize_with(index + 1, Default::default);
                }
                sets[index].0 = columns.into_iter().map(|c| c.name).collect();
            }
            QueryEvent::Rows { index, rows } => {
                if sets.len() <= index {
                    sets.resize_with(index + 1, Default::default);
                }
                sets[index].1.extend(rows);
            }
            QueryEvent::Error { message } => panic!("{text}: {message}"),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    sets
}

#[tokio::test]
async fn databases_scripts_and_tools_run_through_the_services() {
    let Some(profile) = profile() else {
        eprintln!("skipping: SQLLY_REDIS_URL not set");
        return;
    };
    assert_eq!(profile.database_type, DatabaseType::Redis);
    let services = Services::new();

    // The connection manager routes by engine.
    services
        .connections
        .test_connection(&profile)
        .await
        .unwrap();
    let databases = services.connections.list_databases(&profile).await.unwrap();
    assert!(databases.iter().any(|d| d.name == "11"), "{databases:?}");

    // A script: one result set per line, in order.
    let sets = run(
        &services,
        &profile,
        "FLUSHDB\nHSET user:1 name ann\nSET user:1:token abc EX 600\nRPUSH queue:jobs a b\nHGETALL user:1\nSQLLY.KEYS * 100",
    )
    .await;
    assert_eq!(sets.len(), 6);
    assert_eq!(sets[4].0, vec!["field", "value"]);
    assert_eq!(
        sets[5].0,
        vec!["key", "type", "ttl_seconds", "complete", "cursor"]
    );
    assert_eq!(sets[5].1.len(), 3);

    // Every dashboard group answers metric/value rows the tiles know.
    for group in Dashboard::metric_groups_for(DashboardDialect::Redis) {
        let sets = run(&services, &profile, group.sql).await;
        assert_eq!(sets[0].0, vec!["metric", "value"], "{}", group.name);
        assert!(!sets[0].1.is_empty(), "{} answered nothing", group.name);
        for row in &sets[0].1 {
            let CellValue::String(metric) = &row[0] else {
                panic!("{}: {row:?}", group.name)
            };
            assert!(
                sqlly_services::admin::dashboard::KEYS.contains(&metric.as_str()),
                "{}: unknown metric {metric}",
                group.name
            );
        }
    }
    let sizes = run(
        &services,
        &profile,
        Dashboard::database_sizes_query_for(DashboardDialect::Redis),
    )
    .await;
    assert_eq!(sizes[0].0[0], "database_name");
    assert!(
        sizes[0]
            .1
            .iter()
            .any(|r| r[0] == CellValue::String("11".into()))
    );

    // Activity Monitor: sessions carry session_id + query_text, blocking is
    // an honest empty set, the slow log and waits answer.
    let sessions = run(
        &services,
        &profile,
        ActivityMonitor::sessions_query_for(ActivityDialect::Redis),
    )
    .await;
    assert!(sessions[0].0.contains(&"session_id".to_string()));
    assert!(sessions[0].0.contains(&"query_text".to_string()));
    assert!(!sessions[0].1.is_empty());
    let blocking = run(
        &services,
        &profile,
        ActivityMonitor::blocking_query_for(ActivityDialect::Redis),
    )
    .await;
    assert!(blocking[0].1.is_empty());
    assert_eq!(blocking[0].0[0], "blocking_session_id");
    let slow = run(
        &services,
        &profile,
        ActivityMonitor::expensive_queries_query_for(ActivityDialect::Redis),
    )
    .await;
    assert!(slow[0].0.contains(&"query_text".to_string()));
    let waits = run(
        &services,
        &profile,
        ActivityMonitor::waits_query_for(ActivityDialect::Redis),
    )
    .await;
    assert_eq!(waits[0].0[0], "wait_type");
    assert!(!waits[0].1.is_empty());

    // Disk Usage: memory per logical database, sampled.
    let disk = run(
        &services,
        &profile,
        DiskUsage::databases_query_for(DiskDialect::Redis),
    )
    .await;
    assert!(disk[0].0.contains(&"estimated_mb".to_string()));
    let eleven = disk[0]
        .1
        .iter()
        .find(|r| r[0] == CellValue::String("11".into()))
        .unwrap();
    assert_eq!(eleven[1], CellValue::I64(3));

    // Server info comes from INFO server.
    let info =
        sqlly_services::server_info::collect_server_info(services.queries.as_ref(), &profile)
            .await
            .unwrap();
    // The fixture may be a fork: the product is whatever INFO names.
    assert!(
        ["Redis", "Valkey", "KeyDB", "DragonflyDB"].contains(&info.product.as_str()),
        "{}",
        info.product
    );
    assert!(!info.version.is_empty());

    // Query Insights routes to the slow log.
    let insights = services
        .admin_tools
        .query_insights_top(
            DatabaseType::Redis,
            "11",
            20,
            sqlly_services::QueryStoreRank::AvgDuration,
            None,
        )
        .unwrap();
    assert_eq!(insights, "SLOWLOG GET 20");

    // The AI/MCP read-only mode refuses writes by command name.
    let mut read_only = profile.clone();
    read_only.enforce_read_only = true;
    assert!(
        sqlly_intel::query::safety::check_connection_policy_for(
            &read_only,
            "HGETALL user:1\nSQLLY.KEYS *"
        )
        .is_ok()
    );
    let refusal = sqlly_intel::query::safety::check_connection_policy_for(&read_only, "DEL user:1")
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("read-only"), "{refusal}");
    let err = services
        .queries
        .execute(&read_only, "DEL user:1", 8)
        .await
        .err()
        .map(|e| e.to_string());
    // The executor refuses up front or the driver does; either way the key
    // survives.
    let still_there = run(&services, &profile, "EXISTS user:1").await;
    assert_eq!(still_there[0].1[0][0], CellValue::I64(1), "{err:?}");

    run(&services, &profile, "FLUSHDB").await;
}
