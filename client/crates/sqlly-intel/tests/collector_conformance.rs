//! ADR-011 collector conformance matrix: every dialect's collector feeds the
//! SAME ModelEngine pipeline (parallel units → staging → merge → atomic
//! snapshot swap) from the SAME logical fixture, and the resulting model must
//! answer identically: tables with ordinal columns and primary keys, the
//! child→parent foreign-key relationship, and the view.
//!
//! SQLite runs hermetically everywhere (through the generic SQL collector,
//! complementing the DDL-parsing reference collector's own tests). PostgreSQL
//! and MySQL/MariaDB are gated on `SQLLY_PG_*` / `SQLLY_MYSQL_*` env vars and
//! skip silently when absent. SQL Server uses the same generic collector +
//! TDS pooled driver and is gated on `SQLLY_SQL_*` via its live config.

use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};
use sqlly_intel::modelengine::pool::PooledDriver;
use sqlly_intel::modelengine::sql_collector::SqlCatalogCollector;
use sqlly_intel::modelengine::{EngineConfig, ModelEngine, SourceConfig};
use sqlly_lang::sqlmodel::{DatabaseType, Dialect, ObjectRef, SqlName};
use std::sync::Arc;

fn engine() -> Arc<ModelEngine> {
    ModelEngine::new(EngineConfig::default())
}

/// Run the fixture DDL through the pooled driver itself — the same transport
/// the collector uses, so the test needs no separate execution stack.
async fn run_ddl(driver: &dyn PooledDriver, statements: &[&str]) {
    let mut conn = driver.connect().await.expect("connect for fixture DDL");
    for sql in statements {
        conn.query(sql, &[])
            .await
            .unwrap_or_else(|e| panic!("fixture DDL failed: {sql}: {e}"));
    }
}

/// The shared assertion matrix, in each dialect's own identifier-folding.
fn assert_model_matrix(snapshot: &sqlly_lang::sqlmodel::ModelSnapshot, dialect: Dialect) {
    let parent = snapshot.resolve(
        &ObjectRef::bare(SqlName::new("sqlly_parent", dialect, false)),
        None,
        None,
    );
    assert_eq!(parent.len(), 1, "sqlly_parent must resolve exactly once");
    let table = parent[0].table().expect("sqlly_parent is a table");
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.spelling()).collect();
    assert_eq!(columns, ["id", "label"], "ordinal columns");
    let pk = table.primary_key.as_ref().expect("primary key collected");
    assert_eq!(pk.columns.len(), 1);
    assert_eq!(pk.columns[0].spelling(), "id");

    let child = snapshot.resolve(
        &ObjectRef::bare(SqlName::new("sqlly_child", dialect, false)),
        None,
        None,
    );
    assert_eq!(child.len(), 1);
    let child_table = child[0].table().expect("sqlly_child is a table");
    assert_eq!(child_table.foreign_keys.len(), 1, "child FK collected");
    let fk = &child_table.foreign_keys[0];
    assert_eq!(fk.columns[0].spelling(), "parent_id");
    assert_eq!(fk.target.name.spelling(), "sqlly_parent");
    assert_eq!(fk.target_columns[0].spelling(), "id");

    let view = snapshot.resolve(
        &ObjectRef::bare(SqlName::new("sqlly_parent_view", dialect, false)),
        None,
        None,
    );
    assert_eq!(view.len(), 1, "view must resolve");
    match &view[0].body {
        sqlly_lang::sqlmodel::ObjectBody::View(body) => {
            assert_eq!(body.columns.len(), 2, "view columns collected");
        }
        other => panic!("sqlly_parent_view is not a view: {other:?}"),
    }
}

#[tokio::test]
async fn sqlite_generic_collector_conformance_hermetic() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("conformance.db").display().to_string();
    let driver = sqlly_intel::modelengine::sqlite_driver::SqliteDriver {
        path: db_path.clone(),
    };
    run_ddl(
        &driver,
        &[
            "CREATE TABLE sqlly_parent (id INTEGER PRIMARY KEY, label TEXT)",
            "CREATE TABLE sqlly_child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES sqlly_parent(id))",
            "CREATE VIEW sqlly_parent_view AS SELECT id, label FROM sqlly_parent",
        ],
    )
    .await;

    let engine = engine();
    engine.add_source(SourceConfig::Database {
        profile: "sqlite-conformance".into(),
        dialect: Dialect::Sqlite,
        driver: Arc::new(sqlly_intel::modelengine::sqlite_driver::SqliteDriver { path: db_path }),
        collector: Arc::new(SqlCatalogCollector::for_database_type(DatabaseType::Sqlite).unwrap()),
    });
    let _ = engine.rebuild().await;
    assert!(
        engine.collection_errors().is_empty(),
        "{:?}",
        engine.collection_errors()
    );
    assert_model_matrix(&engine.snapshot(), Dialect::Sqlite);
}

fn pg_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_PG_SERVER").ok()?;
    let user = std::env::var("SQLLY_PG_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "pg-collector",
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
async fn postgres_collector_conformance_live() {
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let driver = sqlly_engines::pg::catalog::PgPooledDriver {
        profile: profile.clone(),
    };
    run_ddl(
        &driver,
        &[
            "DROP SCHEMA IF EXISTS sqlly_coll CASCADE",
            "CREATE SCHEMA sqlly_coll",
            "CREATE TABLE sqlly_coll.sqlly_parent (id INT PRIMARY KEY, label TEXT)",
            "CREATE TABLE sqlly_coll.sqlly_child (id INT PRIMARY KEY, parent_id INT NOT NULL REFERENCES sqlly_coll.sqlly_parent(id))",
            "CREATE VIEW sqlly_coll.sqlly_parent_view AS SELECT id, label FROM sqlly_coll.sqlly_parent",
        ],
    )
    .await;

    let engine = engine();
    engine.add_source(SourceConfig::Database {
        profile: "pg-conformance".into(),
        dialect: Dialect::Postgres,
        driver: Arc::new(sqlly_engines::pg::catalog::PgPooledDriver {
            profile: profile.clone(),
        }),
        collector: Arc::new(
            SqlCatalogCollector::for_database_type(DatabaseType::Postgres).unwrap(),
        ),
    });
    let _ = engine.rebuild().await;
    assert!(
        engine.collection_errors().is_empty(),
        "{:?}",
        engine.collection_errors()
    );
    assert_model_matrix(&engine.snapshot(), Dialect::Postgres);

    let cleanup = sqlly_engines::pg::catalog::PgPooledDriver { profile };
    run_ddl(&cleanup, &["DROP SCHEMA sqlly_coll CASCADE"]).await;
}

fn mysql_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
    let database = std::env::var("SQLLY_MYSQL_DATABASE").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "mysql-collector",
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
    p.database = Some(database);
    p.database_type = DatabaseType::MySql;
    p.encrypt = false;
    Some(p)
}

#[tokio::test]
async fn mysql_collector_conformance_live() {
    let Some(profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let driver = sqlly_engines::mysql::catalog::MySqlPooledDriver {
        profile: profile.clone(),
    };
    run_ddl(
        &driver,
        &[
            "DROP VIEW IF EXISTS sqlly_parent_view",
            "DROP TABLE IF EXISTS sqlly_child",
            "DROP TABLE IF EXISTS sqlly_parent",
            "CREATE TABLE sqlly_parent (id INT PRIMARY KEY, label TEXT)",
            "CREATE TABLE sqlly_child (id INT PRIMARY KEY, parent_id INT NOT NULL, FOREIGN KEY (parent_id) REFERENCES sqlly_parent(id))",
            "CREATE VIEW sqlly_parent_view AS SELECT id, label FROM sqlly_parent",
        ],
    )
    .await;

    let engine = engine();
    engine.add_source(SourceConfig::Database {
        profile: "mysql-conformance".into(),
        dialect: Dialect::MySql,
        driver: Arc::new(sqlly_engines::mysql::catalog::MySqlPooledDriver {
            profile: profile.clone(),
        }),
        collector: Arc::new(SqlCatalogCollector::for_database_type(DatabaseType::MySql).unwrap()),
    });
    let _ = engine.rebuild().await;
    assert!(
        engine.collection_errors().is_empty(),
        "{:?}",
        engine.collection_errors()
    );
    assert_model_matrix(&engine.snapshot(), Dialect::MySql);

    let cleanup = sqlly_engines::mysql::catalog::MySqlPooledDriver { profile };
    run_ddl(
        &cleanup,
        &[
            "DROP VIEW sqlly_parent_view",
            "DROP TABLE sqlly_child",
            "DROP TABLE sqlly_parent",
        ],
    )
    .await;
}

fn tds_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_SQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_SQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "tds-collector",
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
    p.database = std::env::var("SQLLY_SQL_DATABASE").ok();
    p.database_type = DatabaseType::SqlServer;
    p.encrypt = false;
    p.trust_server_certificate = true;
    Some(p)
}

#[tokio::test]
async fn sqlserver_collector_conformance_live() {
    let Some(profile) = tds_profile() else {
        eprintln!("skipping: SQLLY_SQL_* env vars not set");
        return;
    };
    let driver = sqlly_engines::tds::catalog::TdsPooledDriver {
        profile: profile.clone(),
    };
    run_ddl(
        &driver,
        &[
            "IF OBJECT_ID('dbo.sqlly_parent_view', 'V') IS NOT NULL DROP VIEW dbo.sqlly_parent_view",
            "IF OBJECT_ID('dbo.sqlly_child', 'U') IS NOT NULL DROP TABLE dbo.sqlly_child",
            "IF OBJECT_ID('dbo.sqlly_parent', 'U') IS NOT NULL DROP TABLE dbo.sqlly_parent",
            "CREATE TABLE dbo.sqlly_parent (id INT PRIMARY KEY, label NVARCHAR(100))",
            "CREATE TABLE dbo.sqlly_child (id INT PRIMARY KEY, parent_id INT NOT NULL FOREIGN KEY REFERENCES dbo.sqlly_parent(id))",
            "CREATE VIEW dbo.sqlly_parent_view AS SELECT id, label FROM dbo.sqlly_parent",
        ],
    )
    .await;

    let engine = engine();
    engine.add_source(SourceConfig::Database {
        profile: "tds-conformance".into(),
        dialect: Dialect::SqlServer,
        driver: Arc::new(sqlly_engines::tds::catalog::TdsPooledDriver {
            profile: profile.clone(),
        }),
        collector: Arc::new(
            SqlCatalogCollector::for_database_type(DatabaseType::SqlServer).unwrap(),
        ),
    });
    let _ = engine.rebuild().await;
    assert!(
        engine.collection_errors().is_empty(),
        "{:?}",
        engine.collection_errors()
    );
    assert_model_matrix(&engine.snapshot(), Dialect::SqlServer);

    let cleanup = sqlly_engines::tds::catalog::TdsPooledDriver { profile };
    run_ddl(
        &cleanup,
        &[
            "DROP VIEW dbo.sqlly_parent_view",
            "DROP TABLE dbo.sqlly_child",
            "DROP TABLE dbo.sqlly_parent",
        ],
    )
    .await;
}
