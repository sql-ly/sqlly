//! Live Oracle driver tests. Opt-in through `SQLLY_ORACLE_SERVER`,
//! `SQLLY_ORACLE_USER`, `SQLLY_ORACLE_PASSWORD` (plus optional
//! `SQLLY_ORACLE_PORT`, default 1521, and `SQLLY_ORACLE_SERVICE`, default
//! `FREEPDB1`); every test skips silently when they are absent. A local
//! `docker run -d --name sqlly-oracle -p 1521:1521 -e ORACLE_PASSWORD=sqlly
//! -e APP_USER=sqlly -e APP_USER_PASSWORD=sqlly gvenzl/oracle-free:slim-faststart`
//! plus the Oracle Instant Client on the machine is enough.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseType, EventBus, EventPayload, SecretProvider, SecretRef,
    SqlParam, StudioError, row_channel,
};
use sqlly_engines::driver::SqlTransport;
use sqlly_engines::oracle::{
    OracleTransport, break_handle, execute_batch_on_client, execute_on_client,
    list_databases_on_client, open_client, query_rows_on_client,
};
use std::time::Duration;

fn live_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "oracle-live",
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

macro_rules! live {
    () => {
        match live_profile() {
            Some(p) => p,
            None => {
                eprintln!("skipping: SQLLY_ORACLE_SERVER/USER not set");
                return;
            }
        }
    };
}

fn payloads(sub: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
    let mut out = Vec::new();
    while let Ok(event) = sub.try_recv() {
        out.push(event.payload);
    }
    out
}

async fn run_collecting(
    client: &sqlly_engines::oracle::OracleClient,
    profile: &ConnectionProfile,
    sql: &str,
) -> (Vec<EventPayload>, Vec<Vec<CellValue>>) {
    let events = EventBus::new(4096);
    let mut sub = events.subscribe();
    let (tx, mut rx) = row_channel(64);
    let drain = tokio::spawn(async move {
        let mut rows = Vec::new();
        while let Some(batch) = rx.recv().await {
            rows.extend(batch.rows);
        }
        rows
    });
    execute_on_client(client, profile, sql, &events, Some(&tx))
        .await
        .unwrap();
    drop(tx);
    let rows = drain.await.unwrap();
    (payloads(&mut sub), rows)
}

#[tokio::test]
async fn live_connect_lists_databases_and_tests_connection() {
    let profile = live!();
    let client = open_client(&profile).await.expect("connect");
    let dbs = list_databases_on_client(&client).await.unwrap();
    assert!(!dbs.is_empty());
    assert!(dbs.iter().all(|d| d.state == "ONLINE"), "{dbs:?}");
    OracleTransport.test_connection(&profile).await.unwrap();
    let listed = OracleTransport.list_databases(&profile).await.unwrap();
    assert_eq!(listed.len(), dbs.len());
}

#[tokio::test]
async fn live_streams_100k_rows_through_the_row_channel() {
    let profile = live!();
    let client = open_client(&profile).await.unwrap();
    let (events, rows) = run_collecting(
        &client,
        &profile,
        "SELECT LEVEL AS n FROM DUAL CONNECT BY LEVEL <= 100000",
    )
    .await;
    assert_eq!(rows.len(), 100_000);
    assert_eq!(rows[0], vec![CellValue::I64(1)]);
    assert_eq!(rows[99_999], vec![CellValue::I64(100_000)]);
    assert!(matches!(events[0], EventPayload::QueryStarted { .. }));
    assert!(matches!(
        &events[1],
        EventPayload::ResultSetStarted { columns, .. } if columns[0].name == "N" && columns[0].data_type == "number"
    ));
    assert!(events.iter().any(|e| matches!(
        e,
        EventPayload::RowCount {
            rows_affected: 100_000,
            ..
        }
    )));
    assert!(matches!(
        events.last(),
        Some(EventPayload::QueryCompleted { .. })
    ));
}

#[tokio::test]
async fn live_value_mapping_covers_oracle_types() {
    let profile = live!();
    let client = open_client(&profile).await.unwrap();
    let sql = "SELECT 42 AS n, 1.5 AS d, 12345678901234567890 AS big, CAST(1.25 AS BINARY_DOUBLE) AS bd, CAST(2.5 AS BINARY_FLOAT) AS bf, 'text' AS s, N'ntext' AS ns, CAST('c' AS CHAR(3)) AS c, TO_CLOB('clob') AS cl, HEXTORAW('DEADBEEF') AS r, TO_BLOB(HEXTORAW('00FF')) AS b, DATE '2024-01-02' AS dt, TIMESTAMP '2024-01-02 03:04:05.678' AS ts, TIMESTAMP '2024-01-02 03:04:05 +02:00' AS tstz, INTERVAL '1 02:03:04' DAY TO SECOND AS ids, INTERVAL '1-2' YEAR TO MONTH AS iym, ROWID AS rid, XMLTYPE('<a/>') AS x, NULL AS nu FROM DUAL";
    let (events, rows) = run_collecting(&client, &profile, sql).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let columns = events
        .iter()
        .find_map(|e| match e {
            EventPayload::ResultSetStarted { columns, .. } => Some(columns.clone()),
            _ => None,
        })
        .unwrap();
    let get = |name: &str| {
        let idx = columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("column {name}"));
        (&columns[idx], &row[idx])
    };
    assert_eq!(get("N").1, &CellValue::I64(42));
    assert_eq!(get("D").1, &CellValue::String("1.5".into()));
    assert_eq!(
        get("BIG").1,
        &CellValue::String("12345678901234567890".into())
    );
    assert_eq!(get("BD").1, &CellValue::F64(1.25));
    assert_eq!(get("BD").0.data_type, "binary_double");
    assert_eq!(get("BF").1, &CellValue::F64(2.5));
    assert_eq!(get("S").1, &CellValue::String("text".into()));
    assert_eq!(get("NS").1, &CellValue::String("ntext".into()));
    assert_eq!(get("C").1, &CellValue::String("c  ".into()));
    assert_eq!(get("CL").1, &CellValue::String("clob".into()));
    assert_eq!(get("CL").0.data_type, "clob");
    assert_eq!(get("R").1, &CellValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    assert_eq!(get("B").1, &CellValue::Bytes(vec![0x00, 0xFF]));
    assert_eq!(get("B").0.data_type, "blob");
    match get("DT").1 {
        CellValue::String(s) => assert!(s.starts_with("2024-01-02"), "{s}"),
        other => panic!("date: {other:?}"),
    }
    match get("TS").1 {
        CellValue::String(s) => assert!(s.starts_with("2024-01-02 03:04:05.678"), "{s}"),
        other => panic!("timestamp: {other:?}"),
    }
    match get("TSTZ").1 {
        CellValue::String(s) => assert!(s.contains("+02:00"), "{s}"),
        other => panic!("timestamp tz: {other:?}"),
    }
    match get("IDS").1 {
        CellValue::String(s) => assert!(s.contains("1 02:03:04"), "{s}"),
        other => panic!("interval ds: {other:?}"),
    }
    match get("IYM").1 {
        CellValue::String(s) => assert!(s.contains("01-02") || s.contains("1-2"), "{s}"),
        other => panic!("interval ym: {other:?}"),
    }
    match get("RID").1 {
        CellValue::String(s) => assert!(!s.is_empty()),
        other => panic!("rowid: {other:?}"),
    }
    match get("X").1 {
        CellValue::String(s) => assert!(s.contains("<a/>"), "{s}"),
        other => panic!("xml: {other:?}"),
    }
    assert_eq!(get("NU").1, &CellValue::Null);
}

#[tokio::test]
async fn live_boolean_maps_on_23ai_and_json_reports_the_hint() {
    let profile = live!();
    let client = open_client(&profile).await.unwrap();
    // BOOLEAN columns exist from 23ai; older servers reject the literal.
    match query_rows_on_client(&client, "SELECT TRUE AS b FROM DUAL", &[]).await {
        Ok(sets) => assert_eq!(sets[0].rows[0][0], CellValue::Bool(true)),
        Err(error) => eprintln!("BOOLEAN unsupported on this server: {error}"),
    }
    // Native JSON cannot be fetched through the client library; the error
    // tells the user what to do instead. Serialized JSON works.
    match query_rows_on_client(&client, "SELECT JSON('{\"a\":1}') AS j FROM DUAL", &[]).await {
        Ok(_) => {}
        Err(error) => assert!(error.to_string().contains("JSON_SERIALIZE"), "{error}"),
    }
    let sets = query_rows_on_client(
        &client,
        "SELECT JSON_SERIALIZE(JSON('{\"a\":1}')) AS j FROM DUAL",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(sets[0].rows[0][0], CellValue::String("{\"a\":1}".into()));
}

#[tokio::test]
async fn live_plsql_blocks_report_dbms_output_and_scripts_report_row_counts() {
    let profile = live!();
    let client = open_client(&profile).await.unwrap();
    execute_batch_on_client(
        &client,
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE sqlly_live_t'; EXCEPTION WHEN OTHERS THEN NULL; END;",
    )
    .await
    .unwrap();
    let script = "CREATE TABLE sqlly_live_t (id NUMBER PRIMARY KEY, name VARCHAR2(20));\n\
                  INSERT INTO sqlly_live_t VALUES (1, 'a');\n\
                  INSERT INTO sqlly_live_t VALUES (2, 'b');\n\
                  BEGIN\n  DBMS_OUTPUT.PUT_LINE('hello');\n  DBMS_OUTPUT.PUT_LINE('world');\n  UPDATE sqlly_live_t SET name = 'c' WHERE id = 2;\nEND;\n/\n\
                  SELECT COUNT(*) AS c FROM sqlly_live_t;\n\
                  EXEC DBMS_OUTPUT.PUT_LINE('from exec');\n\
                  DROP TABLE sqlly_live_t";
    let (events, rows) = run_collecting(&client, &profile, script).await;
    assert_eq!(rows, vec![vec![CellValue::I64(2)]]);
    let messages: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            EventPayload::Message { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec!["hello", "world", "from exec"]);
    let counts: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            EventPayload::RowCount { rows_affected, .. } => Some(*rows_affected),
            _ => None,
        })
        .collect();
    // CREATE, INSERT, INSERT, block (PL/SQL never reports rows), EXEC, DROP,
    // then the SELECT set's own count, which the streamer closes at finish.
    assert_eq!(counts, vec![0, 1, 1, 0, 0, 0, 1], "{counts:?}");
}

#[tokio::test]
async fn live_sql_errors_are_reported_and_the_session_survives() {
    let profile = live!();
    let client = open_client(&profile).await.unwrap();
    let events = EventBus::new(256);
    let _sub = events.subscribe();
    let err = execute_on_client(
        &client,
        &profile,
        "SELECT * FROM sqlly_no_such_table",
        &events,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, StudioError::QueryExecution(ref m) if m.contains("ORA-00942")),
        "{err}"
    );
    let sets = query_rows_on_client(&client, "SELECT 1 FROM DUAL", &[])
        .await
        .unwrap();
    assert_eq!(sets[0].rows[0][0], CellValue::I64(1));
}

#[tokio::test]
async fn live_query_rows_binds_parameters_by_position() {
    let profile = live!();
    let client = open_client(&profile).await.unwrap();
    let sets = query_rows_on_client(
        &client,
        "SELECT :1 + 1 AS n, :2 AS s, :3 AS f FROM DUAL",
        &[
            SqlParam::I64(41),
            SqlParam::String("x".into()),
            SqlParam::F64(1.5),
        ],
    )
    .await
    .unwrap();
    assert_eq!(sets[0].rows[0][0], CellValue::I64(42));
    assert_eq!(sets[0].rows[0][1], CellValue::String("x".into()));
    assert_eq!(sets[0].rows[0][2], CellValue::String("1.5".into()));
}

#[tokio::test]
async fn live_break_execution_cancels_a_running_statement() {
    let profile = live!();
    let client = open_client(&profile).await.unwrap();
    let handle = break_handle(&client);
    let events = EventBus::new(256);
    let _sub = events.subscribe();
    let sql = "SELECT COUNT(*) FROM (SELECT LEVEL FROM DUAL CONNECT BY LEVEL <= 100000000) a, (SELECT LEVEL FROM DUAL CONNECT BY LEVEL <= 100000) b";
    let task_profile = profile.clone();
    let task = tokio::spawn(async move {
        let result = execute_on_client(&client, &task_profile, sql, &events, None).await;
        (client, result)
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    handle.break_execution();
    let (client, result) = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("cancel must end the statement promptly")
        .unwrap();
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("ORA-01013"),
        "expected user-requested cancel, got {err}"
    );
    // The session is still usable after a break.
    let sets = query_rows_on_client(&client, "SELECT 7 FROM DUAL", &[])
        .await
        .unwrap();
    assert_eq!(sets[0].rows[0][0], CellValue::I64(7));
}

#[tokio::test]
async fn live_rollback_policy_leaves_no_trace_and_refuses_ddl() {
    let profile = live!();
    let plain = open_client(&profile).await.unwrap();
    execute_batch_on_client(
        &plain,
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE sqlly_live_rb'; EXCEPTION WHEN OTHERS THEN NULL; END;",
    )
    .await
    .unwrap();
    execute_batch_on_client(&plain, "CREATE TABLE sqlly_live_rb (id NUMBER)")
        .await
        .unwrap();

    let mut wrapped = profile.clone();
    wrapped.auto_wrap_rollback = true;
    let client = open_client(&wrapped).await.unwrap();
    let (_, rows) = run_collecting(
        &client,
        &wrapped,
        "INSERT INTO sqlly_live_rb VALUES (1);\nSELECT COUNT(*) AS c FROM sqlly_live_rb",
    )
    .await;
    // Visible inside the wrapped run, undone by its trailing ROLLBACK.
    assert_eq!(rows, vec![vec![CellValue::I64(1)]]);
    let after = query_rows_on_client(&plain, "SELECT COUNT(*) FROM sqlly_live_rb", &[])
        .await
        .unwrap();
    assert_eq!(after[0].rows[0][0], CellValue::I64(0));
    // DDL would commit the wrap away, so it is refused up front.
    let events = EventBus::new(64);
    let err = execute_on_client(&client, &wrapped, "DROP TABLE sqlly_live_rb", &events, None)
        .await
        .unwrap_err();
    assert!(matches!(err, StudioError::PermissionDenied(_)), "{err}");
    execute_batch_on_client(&plain, "DROP TABLE sqlly_live_rb")
        .await
        .unwrap();
}

#[tokio::test]
async fn live_unknown_service_reports_oracles_own_error() {
    let mut profile = live!();
    profile.database = Some("SQLLY_NO_SUCH_SERVICE".into());
    let err = open_client(&profile).await.unwrap_err();
    assert!(matches!(err, StudioError::Connection(_)), "{err}");
    assert!(err.to_string().contains("ORA-"), "{err}");
    let mut profile = live!();
    profile.authentication = sqlly_core::AuthenticationType::SqlLogin {
        username: "sqlly_nobody".into(),
        secret_ref: SecretRef {
            provider: SecretProvider::InMemory,
            key: "wrong".into(),
        },
    };
    let err = open_client(&profile).await.unwrap_err();
    assert!(matches!(err, StudioError::Authentication(_)), "{err}");
}
