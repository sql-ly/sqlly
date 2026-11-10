//! Golden tests for CREATE/DROP DATABASE builders in `sqlly_intel::metadata::database_ddl`.

use std::path::PathBuf;

use sqlly_core::DatabaseType;
use sqlly_intel::metadata::database_ddl::{
    CreateDatabaseSpec, DropDatabaseSpec, collations_sql, create_database_sql, drop_database_sql,
};
use sqlly_testkit::GoldenTest;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/database_ddl")
}

fn assert_golden(fn_name: &str, case: &str, actual: &str) {
    let path = golden_dir().join(format!("{fn_name}__{case}.sql"));
    let ok = GoldenTest::compare(actual, &path)
        .unwrap_or_else(|e| panic!("golden read failed for {}: {e}", path.display()));
    assert!(
        ok,
        "golden mismatch for {} — re-run with SQLLY_UPDATE_GOLDEN=1",
        path.display()
    );
}

#[test]
fn create_database_sql_golden() {
    assert_golden(
        "create",
        "pg_full",
        &create_database_sql(
            DatabaseType::Postgres,
            &CreateDatabaseSpec {
                name: "plan217".into(),
                owner: Some("app_owner".into()),
                encoding: Some("UTF8".into()),
                collation: Some("en_US.utf8".into()),
                template: Some("template0".into()),
            },
        )
        .unwrap(),
    );
    assert_golden(
        "create",
        "pg_minimal",
        &create_database_sql(
            DatabaseType::Postgres,
            &CreateDatabaseSpec {
                name: "simple_db".into(),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "create",
        "pg_odd_names",
        &create_database_sql(
            DatabaseType::Postgres,
            &CreateDatabaseSpec {
                name: "my \"db\"".into(),
                owner: Some("ro\"le".into()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "create",
        "mysql_full",
        &create_database_sql(
            DatabaseType::MySql,
            &CreateDatabaseSpec {
                name: "plan217".into(),
                encoding: Some("utf8mb4".into()),
                collation: Some("utf8mb4_unicode_ci".into()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "create",
        "mysql_odd_names",
        &create_database_sql(
            DatabaseType::MySql,
            &CreateDatabaseSpec {
                name: "my`db".into(),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "create",
        "tds_full",
        &create_database_sql(
            DatabaseType::SqlServer,
            &CreateDatabaseSpec {
                name: "plan217".into(),
                collation: Some("SQL_Latin1_General_CP1_CI_AS".into()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "create",
        "tds_odd_names",
        &create_database_sql(
            DatabaseType::SqlServer,
            &CreateDatabaseSpec {
                name: "My]Db".into(),
                ..Default::default()
            },
        )
        .unwrap(),
    );
}

#[test]
fn drop_database_sql_golden() {
    assert_golden(
        "drop",
        "pg_force_if_exists",
        &drop_database_sql(
            DatabaseType::Postgres,
            &DropDatabaseSpec {
                name: "plan217".into(),
                force: true,
                if_exists: true,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "drop",
        "pg_plain",
        &drop_database_sql(
            DatabaseType::Postgres,
            &DropDatabaseSpec {
                name: "plan217".into(),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "drop",
        "mysql_if_exists",
        &drop_database_sql(
            DatabaseType::MySql,
            &DropDatabaseSpec {
                name: "plan217".into(),
                if_exists: true,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "drop",
        "tds_single_user",
        &drop_database_sql(
            DatabaseType::SqlServer,
            &DropDatabaseSpec {
                name: "plan217".into(),
                single_user: true,
                if_exists: true,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    assert_golden(
        "drop",
        "tds_plain",
        &drop_database_sql(
            DatabaseType::SqlServer,
            &DropDatabaseSpec {
                name: "plan217".into(),
                ..Default::default()
            },
        )
        .unwrap(),
    );
}

#[test]
fn collations_sql_golden() {
    assert_golden(
        "collations",
        "pg",
        collations_sql(DatabaseType::Postgres).as_deref().unwrap(),
    );
    assert_golden(
        "collations",
        "mysql",
        collations_sql(DatabaseType::MySql).as_deref().unwrap(),
    );
    assert_golden(
        "collations",
        "tds",
        collations_sql(DatabaseType::SqlServer).as_deref().unwrap(),
    );
}
