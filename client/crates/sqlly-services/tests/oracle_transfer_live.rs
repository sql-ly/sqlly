//! Data Transfer into a live Oracle Database: a SQLite table is copied
//! through the real transfer runner and the native Oracle sink in every load
//! mode (create, truncate-then-insert, upsert, append), and the rows are
//! read back to prove the `INSERT ALL` / `MERGE` batches and the session
//! date formats work end to end.
//!
//! Opt-in through `SQLLY_ORACLE_SERVER` / `SQLLY_ORACLE_USER` /
//! `SQLLY_ORACLE_PASSWORD` (optional `SQLLY_ORACLE_PORT`, `SQLLY_ORACLE_SERVICE`,
//! default `FREEPDB1`), exactly like the other Oracle live tests. The scratch
//! table lives in the login's own schema under a `SQLLY_XFER_` name and is
//! dropped at the end.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_engines::oracle::{
    OracleClient, execute_batch_on_client as oracle_batch, open_client as open_oracle,
    query_rows_on_client as oracle_rows,
};
use sqlly_engines::sqlite::{execute_batch_on_client as sqlite_batch, open_client as open_sqlite};
use sqlly_services::{
    Services,
    transfer::{
        plan::{NameCasing, TransferColumn, TransferMode, TransferPlan, TransferTable},
        run::{TransferCancel, TransferOutcome, TransferProgress, run_transfer},
        sink::open_sink,
    },
};
use tokio::sync::mpsc;

const TABLE: &str = "SQLLY_XFER_ORDERS";

fn oracle_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "oracle-transfer",
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

fn sqlite_profile(path: &std::path::Path) -> ConnectionProfile {
    let mut p = ConnectionProfile::sql_login(
        "sqlite-transfer-source",
        path.to_string_lossy().to_string(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    p.database_type = DatabaseType::Sqlite;
    p
}

/// The source table as the transfer wizard would describe it: a key, a
/// bounded string, a fixed-point amount, a timestamp kept as text by SQLite,
/// and a 0/1 flag.
fn orders_table() -> TransferTable {
    let column = |name: &str, source_type: &str, nullable: bool, pk: Option<i64>| TransferColumn {
        name: name.into(),
        source_type: source_type.into(),
        nullable,
        is_identity: false,
        pk_ordinal: pk,
    };
    TransferTable {
        schema: None,
        name: TABLE.into(),
        columns: vec![
            column("ID", "INTEGER", false, Some(1)),
            column("NAME", "VARCHAR(40)", false, None),
            column("AMOUNT", "DECIMAL(10,2)", true, None),
            column("PLACED", "DATETIME", true, None),
            column("ACTIVE", "BOOLEAN", false, None),
        ],
        depends_on: Vec::new(),
    }
}

fn plan(mode: TransferMode) -> TransferPlan {
    TransferPlan {
        source: DatabaseType::Sqlite,
        target: DatabaseType::Oracle,
        target_schema: None,
        mode,
        casing: NameCasing::AsIs,
        tables: vec![orders_table()],
    }
}

async fn transfer(
    source: &Services,
    source_profile: &ConnectionProfile,
    target_profile: &ConnectionProfile,
    mode: TransferMode,
) -> TransferOutcome {
    let mut sink = open_sink(target_profile).await.expect("open Oracle sink");
    let (tx, mut rx) = mpsc::channel::<TransferProgress>(64);
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    let outcome = run_transfer(
        source,
        source_profile,
        &mut sink,
        &plan(mode),
        tx,
        TransferCancel::default(),
    )
    .await;
    let events = drain.await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TransferProgress::Finished(_))),
        "{events:?}"
    );
    assert_eq!(outcome.error, None, "{mode:?}: {:?}", outcome.warnings);
    assert!(!outcome.cancelled);
    outcome
}

async fn read_back(client: &OracleClient) -> Vec<Vec<CellValue>> {
    let sql = format!(
        "SELECT \"ID\", \"NAME\", TO_CHAR(\"AMOUNT\"), TO_CHAR(\"PLACED\", 'YYYY-MM-DD HH24:MI:SS'), \"ACTIVE\" FROM \"{TABLE}\" ORDER BY \"ID\""
    );
    oracle_rows(client, &sql, &[])
        .await
        .unwrap_or_else(|e| panic!("read back failed:\n{sql}\n{e}"))
        .remove(0)
        .rows
}

fn text(cell: &CellValue) -> String {
    match cell {
        CellValue::String(s) => s.clone(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::Bool(b) => (*b as i32).to_string(),
        CellValue::Null => String::new(),
        CellValue::Bytes(_) => "<bytes>".into(),
    }
}

fn row_texts(rows: &[Vec<CellValue>]) -> Vec<Vec<String>> {
    rows.iter().map(|r| r.iter().map(text).collect()).collect()
}

async fn drop_scratch(client: &OracleClient) {
    let _ = oracle_batch(
        client,
        &format!(
            "BEGIN EXECUTE IMMEDIATE 'DROP TABLE \"{TABLE}\" PURGE'; EXCEPTION WHEN OTHERS THEN NULL; END;"
        ),
    )
    .await;
}

#[tokio::test]
async fn sqlite_table_transfers_into_oracle_in_every_mode() {
    let Some(target_profile) = oracle_profile() else {
        eprintln!("skipping: SQLLY_ORACLE_SERVER/USER not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.sqlite");
    let source_profile = sqlite_profile(&path);
    let sqlite = open_sqlite(&source_profile).await.expect("open sqlite");
    sqlite_batch(
        &sqlite,
        &format!(
            "CREATE TABLE \"{TABLE}\" (\"ID\" INTEGER PRIMARY KEY, \"NAME\" VARCHAR(40) NOT NULL, \"AMOUNT\" DECIMAL(10,2), \"PLACED\" DATETIME, \"ACTIVE\" BOOLEAN NOT NULL);\n\
             INSERT INTO \"{TABLE}\" VALUES (1, 'Ada', 150.25, '2024-01-02 03:04:05', 1);\n\
             INSERT INTO \"{TABLE}\" VALUES (2, 'Grace', NULL, '2024-02-03', 0);\n\
             INSERT INTO \"{TABLE}\" VALUES (3, 'O''Hara', 0.5, NULL, 1);"
        ),
    )
    .await
    .expect("seed sqlite");
    drop(sqlite);

    let oracle = open_oracle(&target_profile)
        .await
        .expect("connect to Oracle");
    drop_scratch(&oracle).await;
    let services = Services::new();

    // 1. Create the table on Oracle and load it.
    let outcome = transfer(
        &services,
        &source_profile,
        &target_profile,
        TransferMode::CreateTable,
    )
    .await;
    assert_eq!(outcome.tables, vec![(TABLE.to_string(), 3)]);
    let rows = row_texts(&read_back(&oracle).await);
    assert_eq!(
        rows,
        vec![
            vec!["1", "Ada", "150.25", "2024-01-02 03:04:05", "1"],
            vec!["2", "Grace", "", "2024-02-03 00:00:00", "0"],
            vec!["3", "O'Hara", ".5", "", "1"],
        ]
    );
    // The mapped DDL landed as Oracle types.
    let types = oracle_rows(
        &oracle,
        &format!(
            "SELECT column_name, data_type, char_length, data_precision, data_scale FROM user_tab_columns WHERE table_name = '{TABLE}' ORDER BY column_id"
        ),
        &[],
    )
    .await
    .unwrap()
    .remove(0)
    .rows;
    let types = row_texts(&types);
    assert_eq!(types[1][1], "VARCHAR2");
    assert_eq!(types[1][2], "40");
    assert_eq!(types[2][1], "NUMBER");
    assert_eq!((types[2][3].as_str(), types[2][4].as_str()), ("10", "2"));
    assert!(types[3][1].starts_with("TIMESTAMP"), "{types:?}");
    assert_eq!(
        (types[4][1].as_str(), types[4][3].as_str()),
        ("NUMBER", "1")
    );

    // 2. Truncate then reload: same three rows, no duplicates.
    let outcome = transfer(
        &services,
        &source_profile,
        &target_profile,
        TransferMode::TruncateThenInsert,
    )
    .await;
    assert_eq!(outcome.tables, vec![(TABLE.to_string(), 3)]);
    assert_eq!(read_back(&oracle).await.len(), 3);

    // 3. Upsert: change two rows and add one at the source.
    let sqlite = open_sqlite(&source_profile).await.unwrap();
    sqlite_batch(
        &sqlite,
        &format!(
            "UPDATE \"{TABLE}\" SET \"NAME\" = 'Grace Hopper', \"AMOUNT\" = 20.5 WHERE \"ID\" = 2;\n\
             UPDATE \"{TABLE}\" SET \"ACTIVE\" = 0 WHERE \"ID\" = 3;\n\
             INSERT INTO \"{TABLE}\" VALUES (4, 'Linus', NULL, '2024-04-05 06:07:08', 1);"
        ),
    )
    .await
    .unwrap();
    drop(sqlite);
    let outcome = transfer(
        &services,
        &source_profile,
        &target_profile,
        TransferMode::UpsertOnPrimaryKey,
    )
    .await;
    assert_eq!(outcome.tables, vec![(TABLE.to_string(), 4)]);
    let rows = row_texts(&read_back(&oracle).await);
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(
        rows[1],
        vec!["2", "Grace Hopper", "20.5", "2024-02-03 00:00:00", "0"]
    );
    assert_eq!(rows[2][4], "0");
    assert_eq!(rows[3], vec!["4", "Linus", "", "2024-04-05 06:07:08", "1"]);

    // 4. Append rows with fresh keys.
    let sqlite = open_sqlite(&source_profile).await.unwrap();
    sqlite_batch(
        &sqlite,
        &format!(
            "DELETE FROM \"{TABLE}\";\n\
             INSERT INTO \"{TABLE}\" VALUES (5, 'Ken', 1, '2024-05-06 07:08:09', 1);\n\
             INSERT INTO \"{TABLE}\" VALUES (6, 'Dennis', 2, NULL, 0);"
        ),
    )
    .await
    .unwrap();
    drop(sqlite);
    let outcome = transfer(
        &services,
        &source_profile,
        &target_profile,
        TransferMode::Append,
    )
    .await;
    assert_eq!(outcome.tables, vec![(TABLE.to_string(), 2)]);
    let rows = row_texts(&read_back(&oracle).await);
    assert_eq!(rows.len(), 6, "{rows:?}");
    assert_eq!(rows[5][1], "Dennis");

    // 5. A failing batch rolls back instead of leaving a partial load: the
    //    same keys appended again violate the primary key.
    let mut sink = open_sink(&target_profile).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<TransferProgress>(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = run_transfer(
        &services,
        &source_profile,
        &mut sink,
        &plan(TransferMode::Append),
        tx,
        TransferCancel::default(),
    )
    .await;
    drain.await.unwrap();
    let error = outcome.error.expect("duplicate keys must fail");
    assert!(error.contains("ORA-00001"), "{error}");
    assert_eq!(read_back(&oracle).await.len(), 6);

    drop_scratch(&oracle).await;
}
