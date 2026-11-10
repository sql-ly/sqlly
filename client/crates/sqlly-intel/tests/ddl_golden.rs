//! Golden tests for pure DDL query builders in `sqlly_intel::metadata::ddl`.

use std::path::PathBuf;

use sqlly_intel::metadata::ddl::{
    object_definition_sql, render_sql_type, sp_helptext_sql, table_ddl_columns_sql,
    table_foreign_keys_ddl_sql, table_indexes_ddl_sql, table_primary_key_sql,
    table_triggers_definition_sql, table_type_columns_sql, type_definition_sql,
};
use sqlly_testkit::GoldenTest;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ddl")
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
fn object_definition_sql_golden() {
    assert_golden(
        "object_definition_sql",
        "standard",
        &object_definition_sql("dbo", "Orders"),
    );
    assert_golden(
        "object_definition_sql",
        "odd_names",
        &object_definition_sql("odd schema", "My]Table"),
    );
}

#[test]
fn sp_helptext_sql_golden() {
    assert_golden(
        "sp_helptext_sql",
        "standard",
        &sp_helptext_sql("dbo", "Orders"),
    );
    assert_golden(
        "sp_helptext_sql",
        "odd_names",
        &sp_helptext_sql("odd schema", "My]Table"),
    );
}

#[test]
fn table_ddl_columns_sql_golden() {
    assert_golden(
        "table_ddl_columns_sql",
        "standard",
        &table_ddl_columns_sql("dbo", "Orders"),
    );
    assert_golden(
        "table_ddl_columns_sql",
        "odd_names",
        &table_ddl_columns_sql("odd schema", "My]Table"),
    );
}

#[test]
fn table_primary_key_sql_golden() {
    assert_golden(
        "table_primary_key_sql",
        "standard",
        &table_primary_key_sql("dbo", "Orders"),
    );
    assert_golden(
        "table_primary_key_sql",
        "odd_names",
        &table_primary_key_sql("odd schema", "My]Table"),
    );
}

#[test]
fn type_definition_sql_golden() {
    assert_golden(
        "type_definition_sql",
        "standard",
        &type_definition_sql("dbo", "OrderId"),
    );
    assert_golden(
        "type_definition_sql",
        "odd_names",
        &type_definition_sql("odd schema", "My]Type"),
    );
}

#[test]
fn table_type_columns_sql_golden() {
    assert_golden(
        "table_type_columns_sql",
        "standard",
        &table_type_columns_sql("dbo", "OrderRows"),
    );
    assert_golden(
        "table_type_columns_sql",
        "odd_names",
        &table_type_columns_sql("odd schema", "My]Rows"),
    );
}

#[test]
fn table_indexes_ddl_sql_golden() {
    assert_golden(
        "table_indexes_ddl_sql",
        "standard",
        &table_indexes_ddl_sql("dbo", "Orders"),
    );
    assert_golden(
        "table_indexes_ddl_sql",
        "odd_names",
        &table_indexes_ddl_sql("odd schema", "My]Table"),
    );
}

#[test]
fn table_foreign_keys_ddl_sql_golden() {
    assert_golden(
        "table_foreign_keys_ddl_sql",
        "standard",
        &table_foreign_keys_ddl_sql("dbo", "Orders"),
    );
    assert_golden(
        "table_foreign_keys_ddl_sql",
        "odd_names",
        &table_foreign_keys_ddl_sql("odd schema", "My]Table"),
    );
}

#[test]
fn table_triggers_definition_sql_golden() {
    assert_golden(
        "table_triggers_definition_sql",
        "standard",
        &table_triggers_definition_sql("dbo", "Orders"),
    );
    assert_golden(
        "table_triggers_definition_sql",
        "odd_names",
        &table_triggers_definition_sql("odd schema", "My]Table"),
    );
}

#[test]
fn render_sql_type_vectors() {
    assert_eq!(
        render_sql_type("nvarchar", Some(100), None, None),
        "nvarchar(50)"
    );
    assert_eq!(
        render_sql_type("nvarchar", Some(-1), None, None),
        "nvarchar(max)"
    );
    assert_eq!(
        render_sql_type("decimal", Some(9), Some(18), Some(2)),
        "decimal(18,2)"
    );
    assert_eq!(render_sql_type("int", None, None, None), "int");
    assert_eq!(
        render_sql_type("datetime2", None, None, Some(3)),
        "datetime2(3)"
    );
}
