//! Live conformance for the dialect-dispatched metadata queries: the same
//! service calls against real engines. SQLite runs hermetically everywhere;
//! PostgreSQL and MySQL/MariaDB are gated on `SQLLY_PG_*` / `SQLLY_MYSQL_*`
//! env vars and skip silently when absent — the standard live-test contract.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use sqlly_core::{ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::metadata::{DefaultMetadataService, MetadataService};
use sqlly_services::query::{DefaultQueryExecutor, QueryEvent, QueryExecutor};

async fn run_sql(executor: &Arc<DefaultQueryExecutor>, profile: &ConnectionProfile, sql: &str) {
    let handle = executor
        .execute(profile, sql, 100)
        .await
        .unwrap_or_else(|e| panic!("execute {sql}: {e}"));
    let mut events = handle.events;
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Error { message } => panic!("query failed: {sql}: {message}"),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
}

fn service() -> (Arc<DefaultQueryExecutor>, DefaultMetadataService) {
    let executor = Arc::new(DefaultQueryExecutor::new());
    let service = DefaultMetadataService::new(executor.clone());
    (executor, service)
}

/// The same behavioral matrix for every dialect: given the standard fixture
/// (parent/child tables with a composite-capable FK, a view), the service
/// must report the tables, the view, the columns in ordinal order, the
/// primary keys, and the child→parent foreign key.
async fn assert_metadata_matrix(
    service: &DefaultMetadataService,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
) {
    let tables = service.list_tables(profile, database, None).await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"sqlly_parent"), "tables: {names:?}");
    assert!(names.contains(&"sqlly_child"), "tables: {names:?}");

    let views = service.list_views(profile, database, None).await.unwrap();
    assert!(
        views.iter().any(|v| v.name == "sqlly_parent_view"),
        "views: {:?}",
        views.iter().map(|v| &v.name).collect::<Vec<_>>()
    );

    let columns = service
        .list_columns(profile, database, schema, "sqlly_parent")
        .await
        .unwrap();
    let column_names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        column_names,
        ["id", "label"],
        "ordinal column order: {column_names:?}"
    );
    let id_detail = columns[0].detail.as_deref().unwrap_or_default();
    assert!(id_detail.contains("not null"), "id detail: {id_detail}");

    let all_columns = service.list_all_columns(profile, database).await.unwrap();
    assert!(
        all_columns
            .iter()
            .any(|c| c.object == "sqlly_child" && c.column == "parent_id"),
        "bulk columns must include sqlly_child.parent_id"
    );

    let pks = service.list_primary_keys(profile, database).await.unwrap();
    assert!(
        pks.iter()
            .any(|k| k.table == "sqlly_parent" && k.column == "id"),
        "primary keys: {:?}",
        pks.iter()
            .map(|k| (&k.table, &k.column))
            .collect::<Vec<_>>()
    );

    let fks = service.list_foreign_keys(profile, database).await.unwrap();
    let fk = fks
        .iter()
        .find(|f| f.from_table == "sqlly_child")
        .unwrap_or_else(|| panic!("child FK missing: {fks:?}"));
    assert_eq!(fk.from_column, "parent_id");
    assert_eq!(fk.to_table, "sqlly_parent");
    assert_eq!(fk.to_column, "id");
}

#[tokio::test]
async fn sqlite_metadata_matrix_is_hermetic() {
    let dir = std::env::temp_dir().join(format!("sqlly-meta-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("meta.db");
    let mut profile = ConnectionProfile::sql_login(
        "sqlite-meta",
        db_path.to_string_lossy().as_ref(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = DatabaseType::Sqlite;

    let (executor, service) = service();
    run_sql(
        &executor,
        &profile,
        "CREATE TABLE sqlly_parent (id INTEGER PRIMARY KEY, label TEXT); \
         CREATE TABLE sqlly_child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES sqlly_parent(id)); \
         CREATE VIEW sqlly_parent_view AS SELECT id, label FROM sqlly_parent;",
    )
    .await;

    let database = db_path.to_string_lossy().to_string();
    let schemas = service.list_schemas(&profile, &database).await.unwrap();
    assert!(schemas.contains(&"main".to_string()), "{schemas:?}");
    assert_metadata_matrix(&service, &profile, &database, "main").await;

    // No routines in SQLite: empty, not an error.
    let routines = service
        .list_programmable_objects(&profile, &database, None)
        .await
        .unwrap();
    assert!(routines.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

fn pg_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_PG_SERVER").ok()?;
    let user = std::env::var("SQLLY_PG_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "pg-meta",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_PG_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_PG_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5432);
    p.database = std::env::var("SQLLY_PG_DATABASE")
        .ok()
        .or(Some("postgres".into()));
    p.database_type = DatabaseType::Postgres;
    p.encrypt = false;
    Some(p)
}

#[tokio::test]
async fn postgres_metadata_matrix_live() {
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let database = profile.database.clone().unwrap();
    let (executor, service) = service();
    run_sql(
        &executor,
        &profile,
        "DROP SCHEMA IF EXISTS sqlly_meta CASCADE",
    )
    .await;
    run_sql(
        &executor,
        &profile,
        "CREATE SCHEMA sqlly_meta; \
         CREATE TABLE sqlly_meta.sqlly_parent (id INT PRIMARY KEY, label TEXT); \
         CREATE TABLE sqlly_meta.sqlly_child (id INT PRIMARY KEY, parent_id INT NOT NULL REFERENCES sqlly_meta.sqlly_parent(id)); \
         CREATE VIEW sqlly_meta.sqlly_parent_view AS SELECT id, label FROM sqlly_meta.sqlly_parent; \
         CREATE FUNCTION sqlly_meta.sqlly_fn(x INT) RETURNS INT AS 'SELECT x + 1' LANGUAGE SQL;",
    )
    .await;

    let schemas = service.list_schemas(&profile, &database).await.unwrap();
    assert!(schemas.contains(&"sqlly_meta".to_string()), "{schemas:?}");
    assert!(schemas.contains(&"public".to_string()), "{schemas:?}");

    assert_metadata_matrix(&service, &profile, &database, "sqlly_meta").await;

    let routines = service
        .list_programmable_objects(&profile, &database, Some("sqlly_meta"))
        .await
        .unwrap();
    assert!(
        routines.iter().any(|r| r.name == "sqlly_fn"),
        "routines: {:?}",
        routines.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    let params = service
        .list_procedure_parameters(&profile, &database, "sqlly_meta", "sqlly_fn")
        .await
        .unwrap();
    assert!(
        params.iter().any(|p| p.name.contains('x')),
        "params: {params:?}"
    );

    run_sql(&executor, &profile, "DROP SCHEMA sqlly_meta CASCADE").await;
}

fn mysql_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "mysql-meta",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_MYSQL_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_MYSQL_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3306);
    p.database = std::env::var("SQLLY_MYSQL_DATABASE").ok();
    p.database_type = DatabaseType::MySql;
    p.encrypt = false;
    Some(p)
}

#[tokio::test]
async fn mysql_metadata_matrix_live() {
    let Some(profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let Some(database) = profile.database.clone() else {
        eprintln!("skipping: SQLLY_MYSQL_DATABASE not set");
        return;
    };
    let (executor, service) = service();
    run_sql(&executor, &profile, "DROP VIEW IF EXISTS sqlly_parent_view").await;
    run_sql(&executor, &profile, "DROP TABLE IF EXISTS sqlly_child").await;
    run_sql(&executor, &profile, "DROP TABLE IF EXISTS sqlly_parent").await;
    run_sql(
        &executor,
        &profile,
        "CREATE TABLE sqlly_parent (id INT PRIMARY KEY, label TEXT); \
         CREATE TABLE sqlly_child (id INT PRIMARY KEY, parent_id INT NOT NULL, FOREIGN KEY (parent_id) REFERENCES sqlly_parent(id)); \
         CREATE VIEW sqlly_parent_view AS SELECT id, label FROM sqlly_parent;",
    )
    .await;

    // MySQL's namespace IS the database.
    let schemas = service.list_schemas(&profile, &database).await.unwrap();
    assert_eq!(schemas, vec![database.clone()], "{schemas:?}");

    assert_metadata_matrix(&service, &profile, &database, &database).await;

    run_sql(&executor, &profile, "DROP VIEW sqlly_parent_view").await;
    run_sql(&executor, &profile, "DROP TABLE sqlly_child").await;
    run_sql(&executor, &profile, "DROP TABLE sqlly_parent").await;
}

async fn assert_ok<T>(result: Result<T, sqlly_core::StudioError>) {
    result.unwrap_or_else(|e| panic!("metadata list failed: {e}"));
}

#[tokio::test]
async fn postgres_metadata_new_kinds_live() {
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let database = profile.database.clone().unwrap();
    let (_, service) = service();

    assert_ok(service.list_sequences(&profile, &database, None).await).await;
    assert_ok(service.list_extensions(&profile, &database).await).await;
    assert_ok(service.list_available_extensions(&profile, &database).await).await;
    assert_ok(
        service
            .list_materialized_views(&profile, &database, None)
            .await,
    )
    .await;
    assert_ok(
        service
            .list_database_triggers(&profile, &database, None)
            .await,
    )
    .await;
}

#[tokio::test]
async fn mysql_metadata_new_kinds_live() {
    let Some(profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let Some(database) = profile.database.clone() else {
        eprintln!("skipping: SQLLY_MYSQL_DATABASE not set");
        return;
    };
    let (_, service) = service();

    assert_ok(service.list_events(&profile, &database, None).await).await;
    assert_ok(service.list_sequences(&profile, &database, None).await).await;
    assert_ok(
        service
            .list_database_triggers(&profile, &database, None)
            .await,
    )
    .await;
}

fn sqlserver_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_SQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_SQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "sql-meta",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_SQL_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_SQL_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1433);
    p.database = std::env::var("SQLLY_SQL_DATABASE")
        .ok()
        .or(Some("master".into()));
    p.database_type = DatabaseType::SqlServer;
    p.encrypt = false;
    Some(p)
}

#[tokio::test]
async fn sqlserver_metadata_new_kinds_live() {
    let Some(profile) = sqlserver_profile() else {
        eprintln!("skipping: SQLLY_SQL_* env vars not set");
        return;
    };
    let database = profile.database.clone().unwrap();
    let (_, service) = service();

    assert_ok(service.list_sequences(&profile, &database, None).await).await;
    assert_ok(service.list_synonyms(&profile, &database, None).await).await;
    assert_ok(
        service
            .list_database_triggers(&profile, &database, None)
            .await,
    )
    .await;
}

#[tokio::test]
async fn sqlite_metadata_new_kinds_live() {
    let dir = std::env::temp_dir().join(format!("sqlly-meta-new-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("meta.db");
    let mut profile = ConnectionProfile::sql_login(
        "sqlite-meta-new",
        db_path.to_string_lossy().as_ref(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = DatabaseType::Sqlite;

    let database = db_path.to_string_lossy().to_string();
    let (_, service) = service();

    assert_ok(
        service
            .list_database_triggers(&profile, &database, None)
            .await,
    )
    .await;

    std::fs::remove_dir_all(&dir).ok();
}
