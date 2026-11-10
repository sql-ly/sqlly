//! Live-server check that an explorer Maintenance statement runs through the
//! ordinary execution path: `UPDATE STATISTICS <table>` rendered by
//! `sqlly_services::maintenance` completes on SQL Server with no error event,
//! and the SELECT-only connection policy refuses the same statement with the
//! usual policy message.
//!
//! Self-skips when no live SQL config is present (`SQLLY_SQL_SERVER` +
//! `SQLLY_SQL_PASSWORD`, or `.sqlly-local-sql.json`).

use sqlly_services::maintenance::{MaintenanceOp, MaintenanceTarget, maintenance_sql};
use sqlly_services::{DefaultQueryExecutor, QueryEvent, QueryExecutor};
use sqlly_testkit::LiveSqlConfig;

async fn drain(handle: &mut sqlly_services::QueryHandle) -> (Vec<String>, Vec<String>) {
    let mut kinds = Vec::new();
    let mut errors = Vec::new();
    while let Some(event) = handle.events.recv().await {
        kinds.push(
            format!("{event:?}")
                .split(' ')
                .next()
                .unwrap_or("")
                .to_string(),
        );
        if let QueryEvent::Error { message } = &event {
            errors.push(message.clone());
        }
    }
    (kinds, errors)
}

#[tokio::test(flavor = "multi_thread")]
async fn update_statistics_runs_through_the_execution_path() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let profile = cfg.to_profile("live-maintenance");
    let queries = DefaultQueryExecutor::new();

    let table = "sqlly_maint_live";
    let setup = format!(
        "IF OBJECT_ID(N'dbo.{table}', N'U') IS NULL CREATE TABLE dbo.{table} (id INT NOT NULL PRIMARY KEY, note NVARCHAR(40) NULL); \
         IF NOT EXISTS (SELECT 1 FROM dbo.{table}) INSERT INTO dbo.{table} (id, note) VALUES (1, N'one'), (2, N'two');"
    );
    let mut handle = queries
        .execute(&profile, &setup, 100)
        .await
        .expect("set up the live table");
    let (_, errors) = drain(&mut handle).await;
    assert!(errors.is_empty(), "setup failed: {errors:?}");

    let target = MaintenanceTarget::Table {
        schema: Some("dbo".into()),
        name: table.into(),
    };
    let sql = maintenance_sql(
        profile.database_type,
        MaintenanceOp::UpdateStatistics,
        &target,
        &[],
    )
    .unwrap();
    assert_eq!(sql, format!("UPDATE STATISTICS [dbo].[{table}];"));

    let mut handle = queries
        .execute(&profile, &sql, 100)
        .await
        .expect("execute UPDATE STATISTICS");
    let (kinds, errors) = drain(&mut handle).await;
    eprintln!("{sql}: {kinds:?}");
    assert!(errors.is_empty(), "UPDATE STATISTICS failed: {errors:?}");
    assert!(
        kinds.iter().any(|k| k.starts_with("Completed")),
        "expected a Completed event, got {kinds:?}"
    );

    // The other table-level statements the menu renders for SQL Server must
    // be syntax the server accepts as-is: DBCC CHECKTABLE's quoted-name form
    // and ALTER INDEX ALL … REBUILD / REORGANIZE.
    for op in [
        MaintenanceOp::CheckIntegrity,
        MaintenanceOp::Reindex,
        MaintenanceOp::Reorganize,
    ] {
        let sql = maintenance_sql(profile.database_type, op, &target, &[]).unwrap();
        let mut handle = queries
            .execute(&profile, &sql, 100)
            .await
            .unwrap_or_else(|e| panic!("execute {sql}: {e}"));
        let (kinds, errors) = drain(&mut handle).await;
        eprintln!("{sql}: {kinds:?}");
        assert!(errors.is_empty(), "{sql} failed: {errors:?}");
        assert!(
            kinds.iter().any(|k| k.starts_with("Completed")),
            "expected a Completed event for {sql}, got {kinds:?}"
        );
    }

    // The same statement on a SELECT-only connection is refused before it
    // reaches the server, with the connection-policy message.
    let mut locked = profile.clone();
    locked.block_all_but_select = true;
    let refused = match queries.execute(&locked, &sql, 100).await {
        Err(error) => error.to_string(),
        Ok(mut handle) => {
            let (_, errors) = drain(&mut handle).await;
            errors.join("\n")
        }
    };
    assert!(
        refused.contains("SELECT statements only"),
        "SELECT-only policy must refuse maintenance: {refused}"
    );

    let mut handle = queries
        .execute(&profile, &format!("DROP TABLE dbo.{table};"), 100)
        .await
        .expect("drop the live table");
    let _ = drain(&mut handle).await;
}
