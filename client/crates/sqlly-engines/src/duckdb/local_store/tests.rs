use super::*;
use sqlly_core::{ConnectionEnvironment, SecretProvider, SecretRef};

fn root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn open(root: &Path, id: &str) -> LocalWorkspace {
    LocalWorkspace::open(id, root, WorkspaceOptions::default()).unwrap()
}

fn open_with(root: &Path, id: &str, quota: WorkspaceQuota) -> LocalWorkspace {
    LocalWorkspace::open(
        id,
        root,
        WorkspaceOptions {
            quota,
            ..WorkspaceOptions::default()
        },
    )
    .unwrap()
}

fn col(name: &str, data_type: &str) -> ResultColumn {
    ResultColumn {
        name: name.to_string(),
        data_type: data_type.to_string(),
        nullable: true,
    }
}

fn s(text: &str) -> CellValue {
    CellValue::String(text.to_string())
}

fn materialize(
    ws: &LocalWorkspace,
    relation: &str,
    engine: DatabaseType,
    columns: &[ResultColumn],
    rows: Vec<Vec<CellValue>>,
) -> LocalResult<MaterializeOutcome> {
    let source = SourceInfo::query(engine, "Test source");
    ws.materialize(
        &MaterializeRequest {
            relation,
            columns,
            source: &source,
            mode: WriteMode::FailIfExists,
        },
        vec![Ok(rows)],
        &CancelToken::new(),
    )
}

fn query(ws: &LocalWorkspace, sql: &str) -> LocalResultSet {
    ws.query(sql, None, &CancelToken::new())
        .unwrap()
        .pop()
        .unwrap()
}

fn scalar(ws: &LocalWorkspace, sql: &str) -> CellValue {
    query(ws, sql).rows.remove(0).remove(0)
}

fn column_types(ws: &LocalWorkspace, relation: &str) -> Vec<String> {
    query(
        ws,
        &format!("SELECT column_type FROM (DESCRIBE {relation})"),
    )
    .rows
    .into_iter()
    .map(|row| match &row[0] {
        CellValue::String(t) => t.clone(),
        other => panic!("{other:?}"),
    })
    .collect()
}

fn numbers(count: i64) -> Vec<Vec<CellValue>> {
    (0..count)
        .map(|i| vec![CellValue::I64(i), s(&format!("row {i}"))])
        .collect()
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

// ── Opening and lockdown ────────────────────────────────────────────────

#[test]
fn open_creates_the_workspace_layout() {
    let root = root();
    let ws = open(root.path(), "nb_1");
    assert!(ws.db_path().is_file());
    assert!(root.path().join("nb_1").join(EXCHANGE_DIR).is_dir());
    assert!(root.path().join("nb_1").join(LAST_USED_FILE).is_file());
    assert!(ws.file_exchange_available());
    assert!(!ws.is_encrypted());
    assert!(ws.relations().unwrap().is_empty());
}

#[test]
fn notebook_ids_must_be_single_safe_path_segments() {
    let root = root();
    for bad in [
        "",
        "../x",
        "a/b",
        "a\\b",
        "a b",
        ".",
        "..",
        "nb:1",
        &"x".repeat(200),
    ] {
        assert!(
            matches!(
                LocalWorkspace::open(bad, root.path(), WorkspaceOptions::default()),
                Err(LocalStoreError::InvalidNotebookId(_))
            ),
            "{bad:?}"
        );
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn queries_cannot_reach_files_extensions_or_settings() {
    let root = root();
    let outside = root.path().join("outside.csv");
    write(&outside, "a,b\n1,2\n");
    let ws = open(root.path(), "locked");
    let cancel = CancelToken::new();

    let read = format!("SELECT * FROM read_csv_auto('{}')", outside.display());
    assert!(matches!(
        ws.query(&read, None, &cancel),
        Err(LocalStoreError::Engine(m)) if m.contains("disabled by configuration")
    ));
    let copy_out = format!("SELECT 1; COPY (SELECT 1) TO '{}'", outside.display());
    for refused in [
        "INSTALL httpfs",
        "LOAD httpfs",
        "SET enable_external_access = true",
        "CREATE TABLE t (x INTEGER)",
        "SELECT 1; DROP TABLE _sqlly_provenance",
        "ATTACH 'other.duckdb'",
        "COMMIT; CREATE TABLE t (x INTEGER)",
        "PRAGMA enable_profiling",
        copy_out.as_str(),
    ] {
        assert!(
            matches!(
                ws.query(refused, None, &cancel),
                Err(LocalStoreError::NotReadOnly(_))
            ),
            "{refused}"
        );
    }

    // The lock holds at the engine level too, for any client on the instance.
    let conn = ws.conn();
    for statement in [
        "SET enable_external_access = true",
        "SET autoinstall_known_extensions = true",
        "SET allowed_directories = ['/']",
        "RESET lock_configuration",
        "LOAD httpfs",
    ] {
        assert!(conn.execute_batch(statement).is_err(), "{statement}");
    }
    assert!(
        conn.execute_batch(&format!(
            "ATTACH {} AS o",
            quote_literal(&path_text(&outside))
        ))
        .is_err()
    );
}

#[test]
fn read_only_transaction_blocks_writes_that_slip_past_the_keyword_screen() {
    let root = root();
    let ws = open(root.path(), "ro");
    let conn = ws.conn();
    let refused = run_read_only(&conn, &["CREATE TABLE sneaky AS SELECT 1 AS x".into()], 10);
    assert!(matches!(refused, Err(LocalStoreError::Engine(m)) if m.contains("read-only")));
    drop(conn);
    assert!(ws.relations().unwrap().is_empty());
}

#[test]
fn an_engine_without_file_access_still_materializes_and_queries() {
    // A proxy engine turns DuckDB file access off for the whole process
    // before the workspace applies its own policy.
    let dir = root();
    let conn = ::duckdb::Connection::open(dir.path().join("w.duckdb")).unwrap();
    conn.execute_batch("SET enable_external_access = false")
        .unwrap();
    let exchange = dir.path().join("exchange");
    std::fs::create_dir(&exchange).unwrap();
    let reachable = apply_workspace_policy(
        &conn,
        &exchange,
        dir.path(),
        &WorkspaceQuota::default(),
        None,
    )
    .unwrap();
    assert!(!reachable);
    assert_eq!(setting(&conn, "lock_configuration").unwrap(), "true");
}

#[tokio::test]
async fn the_app_execute_path_joins_the_locked_instance_read_only() {
    let root = root();
    let ws = open(root.path(), "app_path");
    materialize(
        &ws,
        "t",
        DatabaseType::Postgres,
        &[col("id", "int4"), col("name", "text")],
        numbers(3),
    )
    .unwrap();

    let profile = ws.connection_profile("Notebook data");
    assert!(profile.block_all_but_select && profile.enforce_read_only);
    let client = super::super::open_client(&profile).await.unwrap();
    let sets = super::super::query_rows_on_client(&client, "SELECT count(*) FROM t", &[])
        .await
        .unwrap();
    assert_eq!(sets[0].rows[0][0], CellValue::I64(3));
    assert!(
        super::super::query_rows_on_client(
            &client,
            "SELECT current_setting('enable_external_access')",
            &[]
        )
        .await
        .unwrap()[0]
            .rows[0][0]
            == CellValue::Bool(false)
    );
    let events = sqlly_core::EventBus::new(16);
    let write =
        super::super::execute_on_client(&client, &profile, "DELETE FROM t", &events, None).await;
    assert!(write.is_err());
    assert_eq!(scalar(&ws, "SELECT count(*) FROM t"), CellValue::I64(3));
}

// ── Ingestion ───────────────────────────────────────────────────────────

#[test]
fn appender_round_trip_keeps_postgres_types_and_values() {
    let root = root();
    let ws = open(root.path(), "pg_round_trip");
    let columns = [
        col("id", "int4"),
        col("big", "int8"),
        col("price", "numeric(10,2)"),
        col("amount", "numeric"),
        col("ratio", "float8"),
        col("flag", "bool"),
        col("label", "text"),
        col("payload", "bytea"),
        col("day", "date"),
        col("at", "timestamp"),
        col("at_tz", "timestamptz"),
        col("uid", "uuid"),
        col("doc", "jsonb"),
        col("span", "interval"),
    ];
    let rows = vec![
        vec![
            CellValue::I64(1),
            CellValue::I64(i64::MAX),
            s("12.34"),
            s("123456789.123456"),
            CellValue::F64(0.1),
            CellValue::Bool(true),
            s("héllo 世界 🎉 \"quoted\" 'single'"),
            CellValue::Bytes(vec![0, 1, 2, 255]),
            s("2024-02-29"),
            s("2024-02-29 13:45:30.123456"),
            s("2024-02-29 13:45:30+00"),
            s("6f9619ff-8b86-d011-b42d-00c04fc964ff"),
            s("{\"a\": [1, 2]}"),
            s("1 day 02:00:00"),
        ],
        vec![CellValue::Null; 14],
        vec![
            CellValue::I64(-2),
            CellValue::I64(i64::MIN),
            s("-0.05"),
            s("-7"),
            CellValue::F64(-1.5e300),
            CellValue::Bool(false),
            s(""),
            CellValue::Bytes(Vec::new()),
            s("1970-01-01"),
            s("1970-01-01 00:00:00"),
            s("2000-06-15 08:00:00+02"),
            s("00000000-0000-0000-0000-000000000000"),
            s("null"),
            s("3 months"),
        ],
    ];
    let outcome = materialize(&ws, "typed", DatabaseType::Postgres, &columns, rows).unwrap();
    assert_eq!(outcome.row_count, 3);
    assert!(outcome.is_complete());
    assert!(
        outcome.columns.iter().all(|c| !c.fell_back_to_text),
        "{:?}",
        outcome.columns
    );
    assert_eq!(
        column_types(&ws, "typed"),
        [
            "INTEGER",
            "BIGINT",
            "DECIMAL(10,2)",
            "DECIMAL(18,6)",
            "DOUBLE",
            "BOOLEAN",
            "VARCHAR",
            "BLOB",
            "DATE",
            "TIMESTAMP",
            "TIMESTAMP WITH TIME ZONE",
            "UUID",
            "JSON",
            "INTERVAL",
        ]
    );

    let result = query(&ws, "SELECT * FROM typed ORDER BY id NULLS LAST");
    let first = &result.rows[1];
    assert_eq!(first[0], CellValue::I64(1));
    assert_eq!(first[1], CellValue::I64(i64::MAX));
    assert_eq!(first[2], s("12.34"));
    assert_eq!(first[3], s("123456789.123456"));
    assert_eq!(first[4], CellValue::F64(0.1));
    assert_eq!(first[5], CellValue::Bool(true));
    assert_eq!(first[6], s("héllo 世界 🎉 \"quoted\" 'single'"));
    assert_eq!(first[7], CellValue::Bytes(vec![0, 1, 2, 255]));
    assert_eq!(first[8], s("2024-02-29"));
    assert_eq!(first[9], s("2024-02-29 13:45:30.123456"));
    assert_eq!(first[10], s("2024-02-29 13:45:30+00:00"));
    assert_eq!(first[11], s("6f9619ff-8b86-d011-b42d-00c04fc964ff"));
    assert_eq!(first[12], s("{\"a\": [1, 2]}"));
    assert_eq!(first[13], s("1 day 02:00:00"));

    let negative = &result.rows[0];
    assert_eq!(negative[1], CellValue::I64(i64::MIN));
    assert_eq!(negative[2], s("-0.05"));
    assert_eq!(negative[3], s("-7.000000"));
    assert_eq!(negative[4], CellValue::F64(-1.5e300));
    assert_eq!(negative[6], s(""));
    assert_eq!(negative[7], CellValue::Bytes(Vec::new()));
    assert_eq!(negative[10], s("2000-06-15 06:00:00+00:00"));

    assert!(result.rows[2].iter().all(|c| *c == CellValue::Null));
}

#[test]
fn sql_server_values_land_with_their_types() {
    let root = root();
    let ws = open(root.path(), "tds");
    let columns = [
        col("id", "int"),
        col("small", "tinyint"),
        col("active", "bit"),
        col("cost", "money"),
        col("rate", "decimal"),
        col("seen", "datetimeoffset"),
        col("made", "datetime2"),
        col("key", "uniqueidentifier"),
        col("name", "nvarchar"),
        col("ver", "timestamp"),
    ];
    let rows = vec![vec![
        CellValue::I64(7),
        CellValue::I64(255),
        CellValue::Bool(true),
        s("1234.5678"),
        s("0.000123"),
        // The TDS driver renders DATETIMEOFFSET as RFC 3339 and DATETIME2
        // through chrono, nanoseconds included.
        s("2024-01-15T10:30:00+02:00"),
        s("2024-01-15 10:30:00.123456700"),
        s("6F9619FF-8B86-D011-B42D-00C04FC964FF"),
        s("Ünïcödé ✓"),
        CellValue::Bytes(vec![0, 0, 0, 0, 0, 0, 7, 209]),
    ]];
    let outcome = materialize(&ws, "orders", DatabaseType::SqlServer, &columns, rows).unwrap();
    assert_eq!(
        column_types(&ws, "orders"),
        [
            "INTEGER",
            "UTINYINT",
            "BOOLEAN",
            "DECIMAL(19,4)",
            "DECIMAL(18,6)",
            "TIMESTAMP WITH TIME ZONE",
            "TIMESTAMP",
            "UUID",
            "VARCHAR",
            "BLOB",
        ],
        "{:?}",
        outcome.columns
    );
    let row = query(&ws, "SELECT * FROM orders").rows.remove(0);
    assert_eq!(row[3], s("1234.5678"));
    assert_eq!(row[4], s("0.000123"));
    assert_eq!(row[5], s("2024-01-15 08:30:00+00:00"));
    assert_eq!(row[7], s("6f9619ff-8b86-d011-b42d-00c04fc964ff"));
    assert_eq!(row[8], s("Ünïcödé ✓"));
    assert_eq!(row[9], CellValue::Bytes(vec![0, 0, 0, 0, 0, 0, 7, 209]));
}

#[test]
fn values_that_do_not_fit_the_mapped_type_keep_the_column_as_text() {
    let root = root();
    let ws = open(root.path(), "fallback");
    let columns = [
        col("n", "int4"),
        col("d", "date"),
        col("ok", "int4"),
        col("whole", "int8"),
    ];
    let rows = vec![
        vec![
            CellValue::I64(1),
            s("2024-01-01"),
            CellValue::I64(1),
            s("1"),
        ],
        vec![s("not a number"), s("someday"), CellValue::I64(2), s("2.5")],
        vec![
            s("99999999999"),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
        ],
    ];
    let outcome = materialize(&ws, "mixed", DatabaseType::Postgres, &columns, rows).unwrap();
    assert!(outcome.columns[0].fell_back_to_text);
    assert!(outcome.columns[1].fell_back_to_text);
    assert!(!outcome.columns[2].fell_back_to_text);
    assert!(
        outcome.columns[3].fell_back_to_text,
        "2.5 must not round into an integer"
    );
    assert_eq!(
        column_types(&ws, "mixed"),
        ["VARCHAR", "VARCHAR", "INTEGER", "VARCHAR"]
    );
    assert_eq!(
        scalar(&ws, "SELECT whole FROM mixed WHERE ok = 2"),
        s("2.5")
    );
    let values = query(&ws, "SELECT n, d FROM mixed").rows;
    assert_eq!(values[1], vec![s("not a number"), s("someday")]);
    assert_eq!(values[2][0], s("99999999999"));
}

#[test]
fn untyped_sqlite_columns_are_inferred() {
    let root = root();
    let ws = open(root.path(), "infer");
    let columns = [col("a", ""), col("b", ""), col("c", ""), col("d", "")];
    let rows = vec![
        vec![
            CellValue::I64(1),
            CellValue::F64(1.5),
            s("x"),
            CellValue::Null,
        ],
        vec![
            CellValue::I64(2),
            CellValue::I64(2),
            CellValue::I64(3),
            CellValue::Null,
        ],
    ];
    materialize(&ws, "inferred", DatabaseType::Sqlite, &columns, rows).unwrap();
    assert_eq!(
        column_types(&ws, "inferred"),
        ["BIGINT", "DOUBLE", "VARCHAR", "VARCHAR"]
    );
}

#[test]
fn duplicate_and_blank_column_names_are_made_unique() {
    let root = root();
    let ws = open(root.path(), "names");
    let columns = [col("id", "int4"), col("", "text"), col("ID", "int4")];
    let rows = vec![vec![CellValue::I64(1), s("x"), CellValue::I64(2)]];
    let outcome = materialize(&ws, "dupes", DatabaseType::Postgres, &columns, rows).unwrap();
    let names: Vec<&str> = outcome.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "column2", "ID_2"]);
    assert_eq!(scalar(&ws, "SELECT \"ID_2\" FROM dupes"), CellValue::I64(2));
}

#[test]
fn short_and_long_rows_are_padded_and_trimmed() {
    let root = root();
    let ws = open(root.path(), "ragged");
    let columns = [col("a", "int4"), col("b", "text")];
    let rows = vec![
        vec![CellValue::I64(1)],
        vec![CellValue::I64(2), s("b"), s("extra")],
    ];
    materialize(&ws, "ragged", DatabaseType::Postgres, &columns, rows).unwrap();
    let rows = query(&ws, "SELECT * FROM ragged ORDER BY a").rows;
    assert_eq!(rows[0], vec![CellValue::I64(1), CellValue::Null]);
    assert_eq!(rows[1], vec![CellValue::I64(2), s("b")]);
}

#[test]
fn row_quota_returns_a_partial_relation() {
    let root = root();
    let ws = open_with(
        root.path(),
        "row_quota",
        WorkspaceQuota {
            max_rows_per_relation: 10,
            ..WorkspaceQuota::default()
        },
    );
    let columns = [col("id", "int4"), col("name", "text")];
    let outcome =
        materialize(&ws, "capped", DatabaseType::Postgres, &columns, numbers(25)).unwrap();
    assert_eq!(outcome.row_count, 10);
    assert_eq!(outcome.partial, Some(PartialReason::RowLimit { limit: 10 }));
    assert_eq!(
        scalar(&ws, "SELECT count(*) FROM capped"),
        CellValue::I64(10)
    );
    let record = ws.provenance("capped").unwrap().unwrap();
    assert!(!record.complete);
    assert_eq!(record.partial_reason, outcome.partial);

    // Exactly at the limit is complete.
    let exact = materialize(&ws, "exact", DatabaseType::Postgres, &columns, numbers(10)).unwrap();
    assert!(exact.is_complete());
}

#[test]
fn byte_quota_returns_a_partial_relation() {
    let root = root();
    let ws = open_with(
        root.path(),
        "byte_quota",
        WorkspaceQuota {
            max_bytes_per_relation: 100,
            ..WorkspaceQuota::default()
        },
    );
    let columns = [col("blob", "text")];
    let rows: Vec<Vec<CellValue>> = (0..10).map(|_| vec![s(&"x".repeat(30))]).collect();
    let outcome = materialize(&ws, "wide", DatabaseType::Postgres, &columns, rows).unwrap();
    assert_eq!(outcome.row_count, 3);
    assert_eq!(outcome.byte_count, 90);
    assert_eq!(
        outcome.partial,
        Some(PartialReason::ByteLimit { limit: 100 })
    );
}

#[test]
fn disk_quota_refuses_new_relations() {
    let root = root();
    let ws = open_with(
        root.path(),
        "disk_quota",
        WorkspaceQuota {
            max_disk_bytes: 1,
            ..WorkspaceQuota::default()
        },
    );
    let result = materialize(
        &ws,
        "t",
        DatabaseType::Postgres,
        &[col("a", "int4")],
        vec![],
    );
    assert!(matches!(
        result,
        Err(LocalStoreError::DiskQuotaExceeded { limit: 1, .. })
    ));
}

#[test]
fn source_marked_incomplete_makes_the_relation_partial() {
    let root = root();
    let ws = open(root.path(), "incomplete");
    let mut source = SourceInfo::query(DatabaseType::Postgres, "pg");
    source.source_incomplete = Some("truncated at 1000 rows".into());
    let columns = [col("id", "int4"), col("name", "text")];
    let outcome = ws
        .materialize(
            &MaterializeRequest {
                relation: "sampled",
                columns: &columns,
                source: &source,
                mode: WriteMode::FailIfExists,
            },
            vec![Ok(numbers(5))],
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(
        outcome.partial,
        Some(PartialReason::SourceIncomplete {
            detail: "truncated at 1000 rows".into()
        })
    );
}

#[test]
fn cancel_mid_ingest_rolls_everything_back() {
    let root = root();
    let ws = open(root.path(), "cancel");
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let mut sent = 0;
    let batches = std::iter::from_fn(move || {
        sent += 1;
        if sent == 3 {
            trigger.cancel();
        }
        Some(Ok(numbers(1000)))
    });
    let source = SourceInfo::query(DatabaseType::Postgres, "pg");
    let columns = [col("id", "int4"), col("name", "text")];
    let result = ws.materialize(
        &MaterializeRequest {
            relation: "half",
            columns: &columns,
            source: &source,
            mode: WriteMode::FailIfExists,
        },
        batches,
        &cancel,
    );
    assert_eq!(result, Err(LocalStoreError::Canceled));
    assert!(ws.relations().unwrap().is_empty());
    assert!(ws.all_provenance().unwrap().is_empty());
    assert_eq!(
        scalar(&ws, "SELECT count(*) FROM duckdb_tables() WHERE temporary"),
        CellValue::I64(0)
    );
}

#[test]
fn a_failing_source_leaves_no_half_table() {
    let root = root();
    let ws = open(root.path(), "source_error");
    let source = SourceInfo::query(DatabaseType::Postgres, "pg");
    let columns = [col("id", "int4"), col("name", "text")];
    let result = ws.materialize(
        &MaterializeRequest {
            relation: "broken",
            columns: &columns,
            source: &source,
            mode: WriteMode::FailIfExists,
        },
        vec![Ok(numbers(10)), Err("connection reset".into())],
        &CancelToken::new(),
    );
    assert_eq!(
        result,
        Err(LocalStoreError::Source("connection reset".into()))
    );
    assert!(ws.relations().unwrap().is_empty());
}

#[test]
fn replace_or_fail_semantics() {
    let root = root();
    let ws = open(root.path(), "modes");
    let columns = [col("id", "int4"), col("name", "text")];
    materialize(&ws, "Orders", DatabaseType::Postgres, &columns, numbers(2)).unwrap();
    assert_eq!(
        materialize(&ws, "orders", DatabaseType::Postgres, &columns, numbers(5)),
        Err(LocalStoreError::RelationExists("orders".into()))
    );
    let source = SourceInfo::query(DatabaseType::Postgres, "pg");
    let replaced = ws
        .materialize(
            &MaterializeRequest {
                relation: "orders",
                columns: &columns,
                source: &source,
                mode: WriteMode::Replace,
            },
            vec![Ok(numbers(5))],
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(replaced.row_count, 5);
    assert_eq!(
        scalar(&ws, "SELECT count(*) FROM ORDERS"),
        CellValue::I64(5)
    );
    assert_eq!(ws.all_provenance().unwrap().len(), 1);

    // A failed replace keeps the old relation.
    let failed = ws.materialize(
        &MaterializeRequest {
            relation: "orders",
            columns: &columns,
            source: &source,
            mode: WriteMode::Replace,
        },
        vec![Err("boom".into())],
        &CancelToken::new(),
    );
    assert!(failed.is_err());
    assert_eq!(
        scalar(&ws, "SELECT count(*) FROM orders"),
        CellValue::I64(5)
    );
}

#[test]
fn relation_names_are_validated() {
    let root = root();
    let ws = open(root.path(), "idents");
    let columns = [col("a", "int4")];
    for bad in [
        "",
        "1abc",
        "a-b",
        "a b",
        "a\"; DROP TABLE x; --",
        "_sqlly_provenance",
        "_SQLLY_mine",
        "ünicode",
        &"a".repeat(64),
    ] {
        assert!(
            matches!(
                materialize(&ws, bad, DatabaseType::Postgres, &columns, vec![]),
                Err(LocalStoreError::InvalidRelationName { .. })
            ),
            "{bad:?}"
        );
    }
    for good in ["a", "_a", "Orders_2024", &"a".repeat(63)] {
        materialize(&ws, good, DatabaseType::Postgres, &columns, vec![]).unwrap();
    }
}

#[test]
fn reserved_words_and_odd_column_names_are_quoted() {
    let root = root();
    let ws = open(root.path(), "quoting");
    let columns = [
        col("select", "int4"),
        col("a \"quoted\" name", "text"),
        col("semi;colon", "text"),
    ];
    let rows = vec![vec![CellValue::I64(1), s("q"), s("s")]];
    materialize(&ws, "order", DatabaseType::Postgres, &columns, rows).unwrap();
    assert_eq!(
        scalar(&ws, "SELECT \"a \"\"quoted\"\" name\" FROM \"order\""),
        s("q")
    );
}

// ── Provenance ──────────────────────────────────────────────────────────

#[test]
fn provenance_rows_describe_every_relation() {
    let root = root();
    let ws = open(root.path(), "prov");
    let mut source = SourceInfo::query(DatabaseType::SqlServer, "Sales (prod)");
    source.profile_id = Some("4f1e0c9a-0000-4000-8000-000000000001".into());
    source.database = Some("Sales".into());
    source.environment = Some("production".into());
    source.sql_hash = Some("sha256:abc".into());
    source.cell_id = Some("c_abcdefghij".into());
    source.run_id = Some("run-1".into());
    source.extracted_at = Some("2026-11-18T10:00:00.000Z".into());
    let columns = [col("id", "int"), col("total", "decimal(12,2)")];
    let outcome = ws
        .materialize(
            &MaterializeRequest {
                relation: "sales",
                columns: &columns,
                source: &source,
                mode: WriteMode::FailIfExists,
            },
            vec![Ok(vec![vec![CellValue::I64(1), s("9.99")]])],
            &CancelToken::new(),
        )
        .unwrap();
    let record = ws.provenance("SALES").unwrap().unwrap();
    assert_eq!(record.relation, "sales");
    assert_eq!(record.source_kind, SourceKind::Query);
    assert_eq!(record.source_label, "Sales (prod)");
    assert_eq!(record.engine.as_deref(), Some("SqlServer"));
    assert_eq!(record.profile_id, source.profile_id);
    assert_eq!(record.database.as_deref(), Some("Sales"));
    assert_eq!(record.environment.as_deref(), Some("production"));
    assert_eq!(record.source_sql_hash.as_deref(), Some("sha256:abc"));
    assert_eq!(record.cell_id.as_deref(), Some("c_abcdefghij"));
    assert_eq!(record.extracted_at, "2026-11-18T10:00:00.000Z");
    assert_eq!(record.row_count, 1);
    assert_eq!(record.byte_count, 8 + 4);
    assert!(record.complete);
    assert_eq!(record.columns, outcome.columns);
    assert_eq!(record.columns[1].local_type, "DECIMAL(12,2)");
    assert!(
        record
            .checksum
            .as_deref()
            .unwrap()
            .starts_with("rows-sha256:")
    );
    assert_eq!(record.checksum, outcome.checksum);

    // Same rows, same checksum; different rows, different checksum.
    let again = materialize(
        &ws,
        "sales_again",
        DatabaseType::SqlServer,
        &columns,
        vec![vec![CellValue::I64(1), s("9.99")]],
    )
    .unwrap();
    assert_eq!(again.checksum, outcome.checksum);
    let other = materialize(
        &ws,
        "sales_other",
        DatabaseType::SqlServer,
        &columns,
        vec![vec![CellValue::I64(2), s("9.99")]],
    )
    .unwrap();
    assert_ne!(other.checksum, outcome.checksum);

    // Queryable from local SQL too.
    assert_eq!(
        scalar(
            &ws,
            "SELECT source_label FROM _sqlly_provenance WHERE relation = 'sales'"
        ),
        s("Sales (prod)")
    );
    assert!(ws.drop_relation("sales").unwrap());
    assert!(ws.provenance("sales").unwrap().is_none());
    assert!(!ws.drop_relation("sales").unwrap());
}

#[test]
fn lineage_follows_upstream_relations() {
    let root = root();
    let ws = open(root.path(), "lineage");
    let columns = [col("id", "int4"), col("name", "text")];
    materialize(&ws, "a", DatabaseType::Postgres, &columns, numbers(1)).unwrap();
    materialize(&ws, "b", DatabaseType::MySql, &columns, numbers(1)).unwrap();
    let mut derived = SourceInfo::query(DatabaseType::DuckDb, "Local cell");
    derived.kind = SourceKind::Local;
    derived.upstream = vec!["a".into(), "B".into()];
    ws.materialize(
        &MaterializeRequest {
            relation: "joined",
            columns: &columns,
            source: &derived,
            mode: WriteMode::FailIfExists,
        },
        vec![Ok(numbers(1))],
        &CancelToken::new(),
    )
    .unwrap();
    let mut names: Vec<String> = ws
        .lineage("joined")
        .unwrap()
        .into_iter()
        .map(|r| r.relation)
        .collect();
    names.sort();
    assert_eq!(names, ["a", "b", "joined"]);
}

// ── Cross-source join without secrets ───────────────────────────────────

fn secret_profile(
    name: &str,
    engine: DatabaseType,
    server: &str,
    user: &str,
    secret: &str,
) -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(
        name,
        server,
        user,
        SecretRef {
            provider: SecretProvider::InMemory,
            key: secret.to_string(),
        },
    );
    profile.database_type = engine;
    profile.database = Some("warehouse".into());
    profile.environment = ConnectionEnvironment::Production;
    profile
}

#[test]
fn cross_database_join_works_without_embedding_secrets() {
    let root = root();
    let ws = open(root.path(), "cross_join");
    let crm = secret_profile(
        "CRM",
        DatabaseType::SqlServer,
        "crm-db.internal;Password=Hunter2!crm",
        "crm_admin_user",
        "keychain-secret-crm-91f2",
    );
    let billing = secret_profile(
        "Billing",
        DatabaseType::Postgres,
        "postgres://billing_user:S3cr3t-billing@billing.internal/db",
        "billing_user",
        "keychain-secret-billing-77aa",
    );

    let customers = SourceInfo::from_profile(&crm);
    ws.materialize(
        &MaterializeRequest {
            relation: "customers",
            columns: &[col("customer_id", "int"), col("name", "nvarchar")],
            source: &customers,
            mode: WriteMode::FailIfExists,
        },
        vec![Ok(vec![
            vec![CellValue::I64(1), s("Ada")],
            vec![CellValue::I64(2), s("Grace")],
        ])],
        &CancelToken::new(),
    )
    .unwrap();
    let invoices = SourceInfo::from_profile(&billing);
    ws.materialize(
        &MaterializeRequest {
            relation: "invoices",
            columns: &[col("customer_id", "int4"), col("amount", "numeric(10,2)")],
            source: &invoices,
            mode: WriteMode::FailIfExists,
        },
        vec![Ok(vec![
            vec![CellValue::I64(1), s("10.50")],
            vec![CellValue::I64(1), s("4.50")],
            vec![CellValue::I64(2), s("7.00")],
        ])],
        &CancelToken::new(),
    )
    .unwrap();

    let joined = query(
        &ws,
        "SELECT c.name, sum(i.amount) AS total FROM customers c \
         JOIN invoices i USING (customer_id) GROUP BY c.name ORDER BY c.name",
    );
    assert_eq!(
        joined.rows,
        vec![vec![s("Ada"), s("15.00")], vec![s("Grace"), s("7.00")]]
    );

    // Provenance names both sources by label and opaque id only.
    let records = ws.all_provenance().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].source_label, "CRM");
    assert_eq!(records[0].profile_id, Some(crm.id.0.to_string()));
    assert_eq!(records[1].source_label, "Billing");
    assert_eq!(records[0].environment.as_deref(), Some("PRODUCTION"));

    let secrets = [
        "Hunter2",
        "S3cr3t",
        "Password",
        "crm_admin_user",
        "billing_user",
        "keychain-secret",
        "crm-db.internal",
        "billing.internal",
        "postgres://",
    ];
    // Every value in every table, provenance included.
    let mut everything = String::new();
    for table in ["customers", "invoices", PROVENANCE_TABLE] {
        for row in query(&ws, &format!("SELECT * FROM {table}")).rows {
            everything.push_str(&format!("{row:?}"));
        }
    }
    let settings = query(
        &ws,
        "SELECT name, CAST(value AS VARCHAR) FROM duckdb_settings()",
    );
    everything.push_str(&format!("{:?}", settings.rows));
    for secret in secrets {
        assert!(
            !everything.contains(secret),
            "{secret} leaked into the workspace"
        );
    }
    assert!(matches!(
        ws.query(
            "CREATE SECRET s (TYPE s3, KEY_ID 'k')",
            None,
            &CancelToken::new()
        ),
        Err(LocalStoreError::NotReadOnly(_))
    ));

    // And nothing in the database file's bytes.
    let path = ws.db_path().to_path_buf();
    ws.conn().execute_batch("CHECKPOINT").unwrap();
    drop(ws);
    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for secret in secrets {
        assert!(
            !text.contains(secret),
            "{secret} found in the database file"
        );
    }
}

// ── Queries and profiling ───────────────────────────────────────────────

#[test]
fn query_caps_rows_across_result_sets() {
    let root = root();
    let ws = open(root.path(), "caps");
    let sets = ws
        .query(
            "SELECT * FROM range(5); SELECT * FROM range(10)",
            Some(7),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].rows.len(), 5);
    assert!(!sets[0].truncated);
    assert_eq!(sets[1].rows.len(), 2);
    assert!(sets[1].truncated);
}

#[test]
fn a_query_can_be_interrupted() {
    let root = root();
    let ws = Arc::new(open(root.path(), "interrupt"));
    let cancel = CancelToken::new();
    let worker = {
        let ws = Arc::clone(&ws);
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            ws.query(
                "SELECT count(*) FROM range(10000000000) a, range(1000) b WHERE a.range % 7 = b.range",
                None,
                &cancel,
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    cancel.cancel();
    assert_eq!(worker.join().unwrap(), Err(LocalStoreError::Canceled));
    assert_eq!(scalar(&ws, "SELECT 42"), CellValue::I64(42));
}

#[test]
fn summarize_reports_column_statistics() {
    let root = root();
    let ws = open(root.path(), "summary");
    materialize(
        &ws,
        "people",
        DatabaseType::Postgres,
        &[col("id", "int4"), col("name", "text")],
        numbers(50),
    )
    .unwrap();
    let summary = ws.summarize("people", &CancelToken::new()).unwrap();
    assert_eq!(summary.rows.len(), 2);
    let names: Vec<&str> = summary.columns.iter().map(|c| c.name.as_str()).collect();
    for expected in [
        "column_name",
        "column_type",
        "min",
        "max",
        "approx_unique",
        "null_percentage",
    ] {
        assert!(names.contains(&expected), "{names:?}");
    }
    let id_row = &summary.rows[0];
    assert_eq!(id_row[0], s("id"));
    assert_eq!(id_row[2], s("0"));
    assert_eq!(id_row[3], s("49"));
    assert!(matches!(
        ws.summarize("missing", &CancelToken::new()),
        Err(LocalStoreError::RelationNotFound(_))
    ));
}

// ── Files ───────────────────────────────────────────────────────────────

fn import(
    ws: &LocalWorkspace,
    relation: &str,
    path: &Path,
    format: ImportFormat,
) -> LocalResult<MaterializeOutcome> {
    ws.import_file(
        &ImportRequest {
            relation,
            path,
            format,
            mode: WriteMode::FailIfExists,
            cell_id: None,
            run_id: None,
        },
        &CancelToken::new(),
    )
}

#[test]
fn csv_json_and_ndjson_imports() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    let ws = open(root.path(), "imports");

    let csv = files.path().join("people.csv");
    write(
        &csv,
        "id,name,joined\n1,Ada,2024-01-02\n2,Grace,2024-03-04\n",
    );
    let outcome = import(&ws, "people", &csv, ImportFormat::Csv).unwrap();
    assert_eq!(outcome.row_count, 2);
    assert_eq!(column_types(&ws, "people"), ["BIGINT", "VARCHAR", "DATE"]);
    let record = ws.provenance("people").unwrap().unwrap();
    assert_eq!(record.source_kind, SourceKind::File);
    assert_eq!(record.source_label, "people.csv");
    assert_eq!(
        record.source_path.as_deref(),
        Some(csv.to_string_lossy().as_ref())
    );
    let expected = {
        use sha2::Digest;
        hex(&sha2::Sha256::digest(std::fs::read(&csv).unwrap()))
    };
    assert_eq!(record.checksum, Some(format!("file-sha256:{expected}")));

    let json = files.path().join("events.json");
    write(
        &json,
        "[{\"id\": 1, \"kind\": \"a\"}, {\"id\": 2, \"kind\": \"b\"}]",
    );
    assert_eq!(
        import(&ws, "events", &json, ImportFormat::Json)
            .unwrap()
            .row_count,
        2
    );

    let ndjson = files.path().join("lines.ndjson");
    write(&ndjson, "{\"x\": 1}\n{\"x\": 2}\n{\"x\": 3}\n");
    assert_eq!(
        import(&ws, "lines", &ndjson, ImportFormat::NdJson)
            .unwrap()
            .row_count,
        3
    );
    assert_eq!(scalar(&ws, "SELECT sum(x) FROM lines"), CellValue::I64(6));

    // The exchange directory is empty again after each import.
    assert_eq!(
        std::fs::read_dir(root.path().join("imports").join(EXCHANGE_DIR))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn sqlite_imports_read_through_sqlite_itself() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    let db = files.path().join("shop.sqlite");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; \
             CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, price NUMERIC(8,2), photo BLOB, misc); \
             INSERT INTO items VALUES (1, 'Tea', 3.5, x'0102', 7), (2, 'Café', 12.25, NULL, 'n/a'); \
             CREATE VIEW cheap AS SELECT * FROM items WHERE price < 5;",
        )
        .unwrap();
    }
    let ws = open(root.path(), "sqlite");
    let outcome = import(
        &ws,
        "items",
        &db,
        ImportFormat::Sqlite {
            table: "items".into(),
        },
    )
    .unwrap();
    assert_eq!(outcome.row_count, 2);
    assert_eq!(
        column_types(&ws, "items"),
        ["BIGINT", "VARCHAR", "DECIMAL(8,2)", "BLOB", "VARCHAR"]
    );
    let row = query(&ws, "SELECT * FROM items WHERE id = 2")
        .rows
        .remove(0);
    assert_eq!(row[1], s("Café"));
    assert_eq!(row[2], s("12.25"));
    let record = ws.provenance("items").unwrap().unwrap();
    assert_eq!(record.engine.as_deref(), Some("Sqlite"));
    assert!(record.checksum.unwrap().starts_with("file-sha256:"));

    let view = import(
        &ws,
        "cheap",
        &db,
        ImportFormat::Sqlite {
            table: "cheap".into(),
        },
    )
    .unwrap();
    assert_eq!(view.row_count, 1);
    assert!(matches!(
        import(
            &ws,
            "nope",
            &db,
            ImportFormat::Sqlite {
                table: "missing".into()
            }
        ),
        Err(LocalStoreError::RelationNotFound(_))
    ));
}

#[test]
fn imports_reject_unsafe_or_unusable_paths() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    let ws = open_with(
        root.path(),
        "rejects",
        WorkspaceQuota {
            max_import_file_bytes: 64,
            ..WorkspaceQuota::default()
        },
    );
    assert_eq!(
        import(&ws, "d", files.path(), ImportFormat::Csv),
        Err(LocalStoreError::PathRejected(
            PathRejection::NotARegularFile
        ))
    );
    assert_eq!(
        import(
            &ws,
            "m",
            &files.path().join("missing.csv"),
            ImportFormat::Csv
        ),
        Err(LocalStoreError::PathRejected(PathRejection::NotFound))
    );
    let big = files.path().join("big.csv");
    write(&big, &"a\n".repeat(100));
    assert_eq!(
        import(&ws, "big", &big, ImportFormat::Csv),
        Err(LocalStoreError::FileTooLarge {
            size: 200,
            limit: 64
        })
    );
    let bad_parquet = files.path().join("bad.parquet");
    write(&bad_parquet, "this is not parquet");
    assert!(matches!(
        import(&ws, "bp", &bad_parquet, ImportFormat::Parquet),
        Err(LocalStoreError::Malformed {
            format: "Parquet",
            ..
        })
    ));
    let bad_json = files.path().join("bad.json");
    write(&bad_json, "{\"unterminated\": [1, 2");
    assert!(matches!(
        import(&ws, "bj", &bad_json, ImportFormat::Json),
        Err(LocalStoreError::Malformed { format: "JSON", .. })
    ));
    let bad_sqlite = files.path().join("bad.db");
    write(&bad_sqlite, "not a sqlite database at all..");
    assert!(matches!(
        import(
            &ws,
            "bs",
            &bad_sqlite,
            ImportFormat::Sqlite { table: "t".into() }
        ),
        Err(LocalStoreError::Malformed {
            format: "SQLite",
            ..
        })
    ));
    assert!(ws.relations().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn imports_and_exports_refuse_symlinks() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    let ws = open(root.path(), "links");
    let secret = files.path().join("secret.txt");
    write(&secret, "id\n1\n");
    let link = files.path().join("innocent.csv");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    assert_eq!(
        import(&ws, "x", &link, ImportFormat::Csv),
        Err(LocalStoreError::PathRejected(PathRejection::Symlink))
    );
    materialize(
        &ws,
        "t",
        DatabaseType::Postgres,
        &[col("a", "int4")],
        vec![],
    )
    .unwrap();
    let out_link = files.path().join("out.parquet");
    std::os::unix::fs::symlink(&secret, &out_link).unwrap();
    assert_eq!(
        ws.export(
            &ExportSource::Relation("t"),
            ExportFormat::Parquet,
            &out_link,
            true,
            &CancelToken::new()
        ),
        Err(LocalStoreError::PathRejected(PathRejection::Symlink))
    );
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "id\n1\n");
}

#[test]
fn compressed_imports_are_bounded_by_the_row_and_byte_quotas() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    // A small gzip that expands to many rows.
    let path = files.path().join("bomb.csv.gz");
    let scratch = ::duckdb::Connection::open_in_memory().unwrap();
    scratch
        .execute_batch(&format!(
            "COPY (SELECT range AS id, 'xxxxxxxxxxxxxxxxxxxxxxxx' AS pad FROM range(200000)) \
             TO {} (FORMAT csv, HEADER true, COMPRESSION gzip)",
            quote_literal(&path_text(&path))
        ))
        .unwrap();
    let compressed = std::fs::metadata(&path).unwrap().len();
    assert!(compressed < 2_000_000, "{compressed}");

    let ws = open_with(
        root.path(),
        "bomb",
        WorkspaceQuota {
            max_rows_per_relation: 1000,
            ..WorkspaceQuota::default()
        },
    );
    let rows = import(&ws, "bomb", &path, ImportFormat::Csv).unwrap();
    assert_eq!(rows.row_count, 1000);
    assert_eq!(rows.partial, Some(PartialReason::RowLimit { limit: 1000 }));
    assert_eq!(
        scalar(&ws, "SELECT count(*) FROM bomb"),
        CellValue::I64(1000)
    );

    let ws = open_with(
        root.path(),
        "bomb_bytes",
        WorkspaceQuota {
            max_bytes_per_relation: 10_000,
            ..WorkspaceQuota::default()
        },
    );
    let bytes = import(&ws, "bomb", &path, ImportFormat::Csv).unwrap();
    assert_eq!(
        bytes.partial,
        Some(PartialReason::ByteLimit { limit: 10_000 })
    );
    assert!(bytes.byte_count <= 10_000, "{}", bytes.byte_count);
    assert!(bytes.row_count > 0 && bytes.row_count < 1000);
}

#[test]
fn parquet_export_import_round_trip() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    let ws = open(root.path(), "parquet");
    let columns = [
        col("id", "int4"),
        col("price", "numeric(10,2)"),
        col("label", "text"),
        col("day", "date"),
        col("raw", "bytea"),
    ];
    let rows = (0..100)
        .map(|i| {
            vec![
                CellValue::I64(i),
                s(&format!("{i}.25")),
                if i % 10 == 0 {
                    CellValue::Null
                } else {
                    s(&format!("ünï {i}"))
                },
                s("2024-05-06"),
                CellValue::Bytes(vec![i as u8, 0, 255]),
            ]
        })
        .collect();
    materialize(&ws, "source", DatabaseType::Postgres, &columns, rows).unwrap();

    let out = files.path().join("export.parquet");
    let exported = ws
        .export(
            &ExportSource::Relation("source"),
            ExportFormat::Parquet,
            &out,
            false,
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(exported.rows, 100);
    assert_eq!(
        exported.bytes_written,
        std::fs::metadata(&out).unwrap().len()
    );
    assert!(exported.bytes_written > 0);

    import(&ws, "back", &out, ImportFormat::Parquet).unwrap();
    assert_eq!(column_types(&ws, "back"), column_types(&ws, "source"));
    assert_eq!(
        scalar(
            &ws,
            "SELECT count(*) FROM ((SELECT * FROM source EXCEPT SELECT * FROM back) \
             UNION ALL (SELECT * FROM back EXCEPT SELECT * FROM source))"
        ),
        CellValue::I64(0)
    );

    // Refuses to clobber unless asked; overwrite replaces the file.
    assert_eq!(
        ws.export(
            &ExportSource::Relation("source"),
            ExportFormat::Parquet,
            &out,
            false,
            &CancelToken::new()
        ),
        Err(LocalStoreError::PathRejected(
            PathRejection::DestinationExists
        ))
    );
    let csv = files.path().join("subset.csv");
    let subset = ws
        .export(
            &ExportSource::Query("SELECT id, label FROM source WHERE id < 3 ORDER BY id"),
            ExportFormat::Csv,
            &csv,
            false,
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(subset.rows, 3);
    let text = std::fs::read_to_string(&csv).unwrap().replace("\r\n", "\n");
    assert_eq!(text, "id,label\n0,\n1,ünï 1\n2,ünï 2\n");
    let replaced = ws
        .export(
            &ExportSource::Query("SELECT 1 AS one"),
            ExportFormat::Csv,
            &csv,
            true,
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(
        replaced.bytes_written,
        std::fs::metadata(&csv).unwrap().len()
    );
    assert_eq!(
        std::fs::read_to_string(&csv).unwrap().replace("\r\n", "\n"),
        "one\n1\n"
    );
}

#[test]
fn exports_are_bounded_and_read_only() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    let ws = open_with(
        root.path(),
        "export_limits",
        WorkspaceQuota {
            max_export_bytes: 100,
            ..WorkspaceQuota::default()
        },
    );
    let cancel = CancelToken::new();
    let out = files.path().join("big.csv");
    assert!(matches!(
        ws.export(
            &ExportSource::Query("SELECT * FROM range(10000)"),
            ExportFormat::Csv,
            &out,
            false,
            &cancel
        ),
        Err(LocalStoreError::ExportTooLarge { limit: 100, .. })
    ));
    assert!(!out.exists());
    for refused in [
        "DELETE FROM x",
        "SELECT 1; SELECT 2",
        "CREATE TABLE y AS SELECT 1",
    ] {
        assert!(
            matches!(
                ws.export(
                    &ExportSource::Query(refused),
                    ExportFormat::Csv,
                    &out,
                    false,
                    &cancel
                ),
                Err(LocalStoreError::NotReadOnly(_))
            ),
            "{refused}"
        );
    }
    assert_eq!(
        ws.export(
            &ExportSource::Relation("missing"),
            ExportFormat::Csv,
            &out,
            false,
            &cancel
        ),
        Err(LocalStoreError::RelationNotFound("missing".into()))
    );
    assert_eq!(
        ws.export(
            &ExportSource::Query("SELECT 1"),
            ExportFormat::Csv,
            &files.path().join("no_such_dir").join("x.csv"),
            false,
            &cancel
        ),
        Err(LocalStoreError::PathRejected(
            PathRejection::DestinationFolderMissing
        ))
    );
    assert_eq!(
        ws.export(
            &ExportSource::Query("SELECT 1"),
            ExportFormat::Csv,
            files.path(),
            true,
            &cancel
        ),
        Err(LocalStoreError::PathRejected(
            PathRejection::NotARegularFile
        ))
    );
}

// ── Close, reopen, encryption ───────────────────────────────────────────

#[test]
fn reopening_right_after_close_sees_the_data() {
    let root = root();
    for round in 0..15 {
        let ws = open(root.path(), "reopen");
        let name = format!("t{round}");
        materialize(
            &ws,
            &name,
            DatabaseType::Postgres,
            &[col("id", "int4"), col("name", "text")],
            numbers(round + 1),
        )
        .unwrap();
        drop(ws);
        let ws = open(root.path(), "reopen");
        assert_eq!(ws.relations().unwrap().len() as i64, round + 1);
        assert_eq!(
            scalar(&ws, &format!("SELECT count(*) FROM {name}")),
            CellValue::I64(round + 1)
        );
    }
}

#[test]
fn concurrent_opens_share_one_instance() {
    let root = root();
    let root_path = root.path().to_path_buf();
    let first = open(&root_path, "shared");
    materialize(
        &first,
        "t",
        DatabaseType::Postgres,
        &[col("id", "int4"), col("name", "text")],
        numbers(4),
    )
    .unwrap();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let root = root_path.clone();
            std::thread::spawn(move || {
                let ws = open(&root, "shared");
                scalar(&ws, "SELECT count(*) FROM t")
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), CellValue::I64(4));
    }
    assert!(super::super::file_in_use(&path_text(first.db_path())));
    let path = first.db_path().to_path_buf();
    drop(first);
    assert!(wait_until_closed(&path, std::time::Duration::from_secs(5)));
    assert!(!super::super::file_in_use(&path_text(&path)));
}

#[test]
fn delete_on_close_removes_the_workspace() {
    let root = root();
    let ws = LocalWorkspace::open(
        "ephemeral",
        root.path(),
        WorkspaceOptions {
            delete_on_close: true,
            ..WorkspaceOptions::default()
        },
    )
    .unwrap();
    materialize(
        &ws,
        "t",
        DatabaseType::Postgres,
        &[col("a", "int4")],
        vec![],
    )
    .unwrap();
    let dir = ws.dir().to_path_buf();
    assert!(ws.close());
    assert!(!dir.exists());
}

#[test]
fn encrypted_workspaces_need_their_key() {
    let root = root();
    let key = EncryptionKey::new("correct horse battery staple");
    let options = |key: Option<EncryptionKey>| WorkspaceOptions {
        encryption_key: key,
        ..WorkspaceOptions::default()
    };
    let ws = LocalWorkspace::open("vault", root.path(), options(Some(key.clone()))).unwrap();
    assert!(ws.is_encrypted());
    assert!(!format!("{ws:?} {key:?}").contains("horse"));
    materialize(
        &ws,
        "secrets",
        DatabaseType::Postgres,
        &[col("marker", "text")],
        vec![vec![s("PLAINTEXT-MARKER-0123456789")]; 50],
    )
    .unwrap();
    ws.conn().execute_batch("CHECKPOINT").unwrap();
    let path = ws.db_path().to_path_buf();
    drop(ws);

    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("PLAINTEXT-MARKER"));
    assert!(!String::from_utf8_lossy(&bytes).contains("horse"));

    assert_eq!(
        LocalWorkspace::open(
            "vault",
            root.path(),
            options(Some(EncryptionKey::new("wrong")))
        )
        .unwrap_err(),
        LocalStoreError::EncryptionKey
    );
    assert_eq!(
        LocalWorkspace::open("vault", root.path(), options(None)).unwrap_err(),
        LocalStoreError::EncryptionKey
    );
    let ws = LocalWorkspace::open("vault", root.path(), options(Some(key))).unwrap();
    assert_eq!(
        scalar(&ws, "SELECT count(*) FROM secrets"),
        CellValue::I64(50)
    );
    assert_eq!(ws.relations().unwrap().len(), 1);
}

#[tokio::test]
async fn encrypted_workspaces_serve_the_app_execute_path() {
    let root = root();
    let ws = LocalWorkspace::open(
        "vault_app",
        root.path(),
        WorkspaceOptions {
            encryption_key: Some(EncryptionKey::new("k")),
            ..WorkspaceOptions::default()
        },
    )
    .unwrap();
    materialize(
        &ws,
        "t",
        DatabaseType::Postgres,
        &[col("a", "int4")],
        vec![vec![CellValue::I64(1)]],
    )
    .unwrap();
    let client = super::super::open_client(&ws.connection_profile("Vault"))
        .await
        .unwrap();
    let sets = super::super::query_rows_on_client(&client, "SELECT count(*) FROM t", &[])
        .await
        .unwrap();
    assert_eq!(sets[0].rows[0][0], CellValue::I64(1));
}

// ── Hygiene ─────────────────────────────────────────────────────────────

#[test]
fn relations_list_sizes_and_clear() {
    let root = root();
    let ws = open(root.path(), "hygiene");
    let columns = [col("id", "int4"), col("name", "text")];
    materialize(
        &ws,
        "big",
        DatabaseType::Postgres,
        &columns,
        numbers(200_000),
    )
    .unwrap();
    materialize(&ws, "small", DatabaseType::Postgres, &columns, numbers(3)).unwrap();
    ws.conn().execute_batch("CHECKPOINT").unwrap();
    let relations = ws.relations().unwrap();
    let names: Vec<&str> = relations.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["big", "small"]);
    assert_eq!(relations[0].row_count, 200_000);
    assert_eq!(relations[0].column_count, 2);
    assert!(relations[0].storage_bytes > relations[1].storage_bytes);
    assert!(relations[0].provenance.is_some());
    assert!(ws.disk_usage() > 0);

    ws.clear().unwrap();
    assert!(ws.relations().unwrap().is_empty());
    assert!(ws.all_provenance().unwrap().is_empty());
}

#[test]
fn lru_eviction_skips_open_and_kept_workspaces() {
    let root = root();
    let set_age = |id: &str, secs_ago: u64| {
        let marker = root.path().join(id).join(LAST_USED_FILE);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(marker)
            .unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago))
            .unwrap();
    };
    for id in ["oldest", "older", "kept", "newest"] {
        let ws = open(root.path(), id);
        materialize(
            &ws,
            "t",
            DatabaseType::Postgres,
            &[col("id", "int4"), col("name", "text")],
            numbers(1000),
        )
        .unwrap();
    }
    set_age("oldest", 4000);
    set_age("older", 3000);
    set_age("kept", 2000);
    set_age("newest", 10);
    let live = open(root.path(), "older");
    set_age("older", 3000);

    let usages = workspace_usages(root.path()).unwrap();
    let order: Vec<&str> = usages.iter().map(|u| u.notebook_id.as_str()).collect();
    assert_eq!(order, ["oldest", "older", "kept", "newest"]);
    assert!(usages[1].open);

    std::fs::create_dir(root.path().join("not a notebook")).unwrap();
    let report = evict_lru(root.path(), 0, &["kept"]).unwrap();
    assert_eq!(report.evicted, ["oldest", "newest"]);
    assert!(report.freed_bytes > 0);
    assert!(!root.path().join("oldest").exists());
    assert!(root.path().join("older").exists());
    assert!(root.path().join("kept").exists());
    assert!(root.path().join("not a notebook").exists());

    assert!(remove_workspace(root.path(), "older").is_err());
    drop(live);
    assert!(wait_until_closed(
        &root.path().join("older").join(WORKSPACE_FILE),
        std::time::Duration::from_secs(5)
    ));
    assert!(remove_workspace(root.path(), "older").unwrap());
    assert!(!remove_workspace(root.path(), "older").unwrap());
    assert!(remove_workspace(root.path(), "../etc").is_err());
}

#[test]
fn local_queries_return_variant_columns_as_text() {
    let dir = root();
    let ws = open(dir.path(), "nb_variant");
    let set = query(&ws, "SELECT 7::VARIANT AS v, 'x' AS t");
    assert_eq!(set.columns[0].name, "v");
    assert_eq!(set.rows[0], vec![s("7"), s("x")]);
    let settings = query(
        &ws,
        "SELECT * FROM duckdb_settings() WHERE name = 'threads'",
    );
    assert_eq!(settings.rows.len(), 1);
}
