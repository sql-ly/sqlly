//! Test Data Generator against a live Oracle Database: the column
//! introspection query runs on a scratch table with every common Oracle type,
//! the parsed columns get sensible generators, and the generated `INSERT ALL`
//! script executes and lands the requested number of rows.
//!
//! Opt-in through `SQLLY_ORACLE_SERVER` / `SQLLY_ORACLE_USER` /
//! `SQLLY_ORACLE_PASSWORD` (optional `SQLLY_ORACLE_PORT`, `SQLLY_ORACLE_SERVICE`,
//! default `FREEPDB1`), exactly like the driver's own live tests. The scratch
//! table is created in the login's own schema and dropped afterwards.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_engines::oracle::{execute_batch_on_client, open_client, query_rows_on_client};
use sqlly_gpui::test_data_generator::{
    ValueGenerator, columns_query, insert_script, parse_columns,
};

const TABLE: &str = "SQLLY_TDG_SAMPLE";

fn live_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "oracle-test-data",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_ORACLE_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_ORACLE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1521);
    p.database = Some(std::env::var("SQLLY_ORACLE_SERVICE").unwrap_or_else(|_| "FREEPDB1".into()));
    p.database_type = DatabaseType::Oracle;
    p.encrypt = false;
    Some(p)
}

fn text(cell: &CellValue) -> String {
    match cell {
        CellValue::String(s) => s.clone(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::Null => String::new(),
        CellValue::Bytes(_) => "<bytes>".into(),
    }
}

#[tokio::test]
async fn oracle_columns_generate_and_insert_all_runs() {
    let Some(profile) = live_profile() else {
        eprintln!("skipping: SQLLY_ORACLE_SERVER/USER not set");
        return;
    };
    let client = open_client(&profile).await.expect("connect to Oracle");
    let drop = format!(
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE {TABLE} PURGE'; EXCEPTION WHEN OTHERS THEN NULL; END;"
    );
    execute_batch_on_client(&client, &drop).await.unwrap();
    execute_batch_on_client(
        &client,
        &format!(
            "CREATE TABLE {TABLE} (\
             id NUMBER(10) GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
             name VARCHAR2(30) NOT NULL, \
             email NVARCHAR2(60), \
             qty NUMBER(3), \
             amount NUMBER(10,2), \
             ratio NUMBER, \
             active NUMBER(1) NOT NULL, \
             born DATE, \
             placed TIMESTAMP(6) NOT NULL, \
             token RAW(16), \
             notes CLOB, \
             total NUMBER(12,2) GENERATED ALWAYS AS (amount * 2) VIRTUAL)"
        ),
    )
    .await
    .expect("create scratch table");

    // Introspection: every column comes back with a bounded type spelling,
    // identity and virtual columns flagged as skipped.
    let sql = columns_query(DatabaseType::Oracle, "", TABLE).expect("Oracle columns query");
    let rows = query_rows_on_client(&client, &sql, &[])
        .await
        .unwrap_or_else(|e| panic!("Oracle rejected the columns query:\n{sql}\n{e}"))
        .remove(0)
        .rows;
    let described: Vec<(String, String)> =
        rows.iter().map(|r| (text(&r[0]), text(&r[1]))).collect();
    assert_eq!(
        described,
        vec![
            ("ID".into(), "NUMBER(10)".into()),
            ("NAME".into(), "VARCHAR2(30)".into()),
            ("EMAIL".into(), "NVARCHAR2(60)".into()),
            ("QTY".into(), "NUMBER(3)".into()),
            ("AMOUNT".into(), "NUMBER(10,2)".into()),
            ("RATIO".into(), "NUMBER".into()),
            ("ACTIVE".into(), "NUMBER(1)".into()),
            ("BORN".into(), "DATE".into()),
            ("PLACED".into(), "TIMESTAMP(6)".into()),
            ("TOKEN".into(), "RAW(16)".into()),
            ("NOTES".into(), "CLOB".into()),
            ("TOTAL".into(), "NUMBER(12,2)".into()),
        ]
    );
    let columns = parse_columns(&rows);
    let by_name = |name: &str| {
        columns
            .iter()
            .find(|(spec, _)| spec.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
    };
    assert!(by_name("ID").0.skip);
    assert!(by_name("TOTAL").0.skip);
    assert!(!by_name("NAME").0.skip && !by_name("NAME").0.nullable);
    assert!(by_name("EMAIL").0.nullable);
    assert_eq!(by_name("NAME").1, ValueGenerator::Name);
    assert_eq!(by_name("EMAIL").1, ValueGenerator::Email);
    assert_eq!(by_name("QTY").1, ValueGenerator::Integer);
    assert_eq!(by_name("AMOUNT").1, ValueGenerator::Decimal);
    assert_eq!(by_name("RATIO").1, ValueGenerator::Decimal);
    assert_eq!(by_name("ACTIVE").1, ValueGenerator::Boolean);
    assert_eq!(by_name("BORN").1, ValueGenerator::Date);
    assert_eq!(by_name("PLACED").1, ValueGenerator::DateTime);
    assert_eq!(by_name("TOKEN").1, ValueGenerator::Binary);
    assert_eq!(by_name("NOTES").1, ValueGenerator::Text);

    // The script executes as-is and lands every row; a second identical run
    // with the same seed produces the same statements.
    let script = insert_script(DatabaseType::Oracle, "", TABLE, &columns, 37, 30, 42).unwrap();
    assert!(script.contains("UNION ALL SELECT "), "{script}");
    assert!(script.contains(" FROM DUAL;"), "{script}");
    assert!(!script.contains("INSERT ALL"), "{script}");
    assert!(!script.contains("\"ID\"") && !script.contains("\"TOTAL\""));
    assert_eq!(
        script,
        insert_script(DatabaseType::Oracle, "", TABLE, &columns, 37, 30, 42).unwrap()
    );
    execute_batch_on_client(&client, &script)
        .await
        .unwrap_or_else(|e| panic!("Oracle rejected the generated script:\n{script}\n{e}"));
    let count = query_rows_on_client(
        &client,
        &format!("SELECT COUNT(*), COUNT(email), MAX(qty), MAX(LENGTH(name)) FROM {TABLE}"),
        &[],
    )
    .await
    .unwrap()
    .remove(0)
    .rows;
    assert_eq!(text(&count[0][0]), "37");
    // 30 % NULLs on a nullable column: some, but not all, rows carry an email.
    let emails: i64 = text(&count[0][1]).parse().unwrap();
    assert!(emails > 0 && emails < 37, "{emails}");
    let max_qty: i64 = text(&count[0][2]).parse().unwrap();
    assert!(max_qty < 1000, "{max_qty}");
    let max_name: i64 = text(&count[0][3]).parse().unwrap();
    assert!(max_name <= 30, "{max_name}");

    // An explicit schema (the login's own) works the same way.
    let owner = text(
        &query_rows_on_client(&client, "SELECT USER FROM DUAL", &[])
            .await
            .unwrap()
            .remove(0)
            .rows[0][0],
    );
    let sql = columns_query(DatabaseType::Oracle, &owner, TABLE).unwrap();
    let rows = query_rows_on_client(&client, &sql, &[])
        .await
        .unwrap()
        .remove(0)
        .rows;
    assert_eq!(rows.len(), 12);

    execute_batch_on_client(&client, &drop).await.unwrap();
}
