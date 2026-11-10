//! Live-server check of `load_table_shape`, the column/primary-key subset of
//! the structure snapshot that model generation reads: engine type spelling
//! with length/precision, nullability, identity, primary key, and default
//! expressions must all come back for a real table.
//!
//! Self-skips when no live SQL config is present.

use sqlly_core::ConnectionProfile;
use sqlly_services::{DefaultQueryExecutor, QueryEvent, QueryExecutor, load_table_shape};
use sqlly_testkit::LiveSqlConfig;

const DB: &str = "tempdb";
const TABLE: &str = "sqlly_shape_probe";

async fn run(queries: &DefaultQueryExecutor, profile: &ConnectionProfile, sql: &str) {
    let mut handle = queries
        .execute(profile, sql, 100)
        .await
        .expect("execute against live server");
    while let Some(event) = handle.events.recv().await {
        if let QueryEvent::Error { message } = event {
            panic!("{sql}: {message}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn table_shape_reports_columns_keys_identity_and_defaults() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let profile = cfg.to_profile("live-table-shape");
    let mut in_db = profile.clone();
    in_db.database = Some(DB.to_string());
    let queries = DefaultQueryExecutor::new();

    run(
        &queries,
        &in_db,
        &format!("IF OBJECT_ID('dbo.{TABLE}') IS NOT NULL DROP TABLE dbo.{TABLE}"),
    )
    .await;
    run(
        &queries,
        &in_db,
        &format!(
            "CREATE TABLE dbo.{TABLE} (\
             id int IDENTITY(1,1) NOT NULL PRIMARY KEY, \
             name nvarchar(50) NULL, \
             total decimal(10,2) NOT NULL DEFAULT 0, \
             created datetime2(3) NOT NULL DEFAULT getdate(), \
             blob varbinary(max) NULL, \
             flag bit NOT NULL DEFAULT 0)"
        ),
    )
    .await;

    let shape = load_table_shape(&queries, &profile, DB, Some("dbo"), TABLE).await;

    run(&queries, &in_db, &format!("DROP TABLE dbo.{TABLE}")).await;

    let shape = shape.expect("load table shape");
    eprintln!("{shape:#?}");
    assert_eq!(shape.table, TABLE);
    assert_eq!(shape.schema.as_deref(), Some("dbo"));
    let names: Vec<&str> = shape.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["id", "name", "total", "created", "blob", "flag"],
        "column order must follow the table"
    );
    assert_eq!(shape.primary_key, vec!["id".to_string()]);

    let col = |n: &str| shape.columns.iter().find(|c| c.name == n).unwrap();
    assert!(col("id").identity && !col("id").nullable);
    assert_eq!(col("id").data_type, "int");
    assert!(col("name").nullable);
    assert_eq!(col("name").data_type, "nvarchar(50)");
    assert_eq!(col("total").data_type, "decimal(10,2)");
    assert!(
        col("total")
            .default
            .as_deref()
            .is_some_and(|d| d.contains('0')),
        "total default: {:?}",
        col("total").default
    );
    assert_eq!(col("created").data_type, "datetime2(3)");
    assert!(
        col("created")
            .default
            .as_deref()
            .is_some_and(|d| d.to_lowercase().contains("getdate")),
        "created default: {:?}",
        col("created").default
    );
    assert_eq!(col("blob").data_type, "varbinary(max)");
    assert!(col("blob").nullable && !col("blob").identity);
    assert_eq!(col("flag").data_type, "bit");
    assert!(col("flag").default.is_some());
}
