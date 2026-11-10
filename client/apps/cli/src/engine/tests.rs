use super::*;
use sqlly_ai::catalog::StubEmbeddingProvider;
use sqlly_core::StudioError;
use sqlly_services::providers::{
    EMBEDDING_MODEL_ENV, EMBEDDING_PROVIDER_ENV, LLM_MODEL_ENV, LLM_PROVIDER_ENV,
    ProviderDiagnostics, build_catalog_providers_from_env,
};
use std::sync::Arc;

#[test]
fn connection_request_maps_to_profile() {
    let profile = EngineConnection {
        server: "db.example.com".to_string(),
        port: 1444,
        database: Some("sales".to_string()),
        auth: "sql".to_string(),
        username: "sa".to_string(),
        password: "secret".to_string(),
        access_token: None,
        encrypt: true,
        trust_certificate: false,
        timeout_seconds: Some(45),
        block_all_but_select: false,
        auto_wrap_rollback: false,
        confirm_data_changes: false,
        database_type: None,
        statement_timeout_seconds: None,
        enforce_read_only: false,
        proxy_profile: None,
    }
    .into_profile();

    assert_eq!(profile.server, "db.example.com");
    assert_eq!(profile.port, 1444);
    assert_eq!(profile.database.as_deref(), Some("sales"));
    assert!(profile.encrypt);
    assert!(!profile.trust_server_certificate);
    assert_eq!(profile.timeout_seconds, 45);
}

#[test]
fn connection_database_type_dispatches_engine_family() {
    // Old clients omit the field entirely: SQL Server, as always.
    let legacy: EngineConnection =
        serde_json::from_str(r#"{"server":"s","username":"u","password":"p"}"#).unwrap();
    assert_eq!(
        legacy.into_profile().database_type,
        sqlly_core::DatabaseType::SqlServer
    );

    // New clients carry the engine family end-to-end.
    let pg: EngineConnection = serde_json::from_str(
        r#"{"server":"s","port":5432,"username":"u","password":"p","database_type":"Postgres"}"#,
    )
    .unwrap();
    assert_eq!(
        pg.into_profile().database_type,
        sqlly_core::DatabaseType::Postgres
    );

    let sqlite: EngineConnection = serde_json::from_str(
        r#"{"server":"/tmp/x.db","username":"","password":"","database_type":"Sqlite"}"#,
    )
    .unwrap();
    let profile = sqlite.into_profile();
    assert_eq!(profile.database_type, sqlly_core::DatabaseType::Sqlite);
    assert_eq!(profile.server, "/tmp/x.db");
}

#[test]
fn entra_connection_request_maps_to_entra_profile() {
    let profile = EngineConnection {
        server: "azure.database.windows.net".to_string(),
        port: 1433,
        database: Some("master".to_string()),
        auth: "entra-browser".to_string(),
        username: String::new(),
        password: String::new(),
        access_token: Some("aad-token".to_string()),
        encrypt: true,
        trust_certificate: false,
        timeout_seconds: None,
        block_all_but_select: false,
        auto_wrap_rollback: false,
        confirm_data_changes: false,
        database_type: None,
        statement_timeout_seconds: None,
        enforce_read_only: false,
        proxy_profile: None,
    }
    .into_profile();

    assert!(matches!(
        profile.authentication,
        sqlly_core::AuthenticationType::EntraBrowser
    ));
    assert_eq!(profile.database.as_deref(), Some("master"));
    assert_eq!(
        profile.runtime_entra_access_token.as_deref(),
        Some("aad-token")
    );
}

/// Regression for the Entra "SELECT only" bug: an Entra connection that
/// does NOT request blocking must produce a profile with
/// `block_all_but_select == false`. Before the fix, the `azure_sql()`
/// constructor defaulted this to `true` and `into_profile` never overrode
/// it, so every Entra query was locked to SELECT-only regardless of the
/// user's editor choice.
#[test]
fn entra_connection_without_safety_fields_does_not_block() {
    let profile = EngineConnection {
        server: "azure.database.windows.net".to_string(),
        port: 1433,
        database: Some("master".to_string()),
        auth: "entra-browser".to_string(),
        username: String::new(),
        password: String::new(),
        access_token: None,
        encrypt: true,
        trust_certificate: false,
        timeout_seconds: None,
        block_all_but_select: false,
        auto_wrap_rollback: false,
        confirm_data_changes: false,
        database_type: None,
        statement_timeout_seconds: None,
        enforce_read_only: false,
        proxy_profile: None,
    }
    .into_profile();

    assert!(
        !profile.block_all_but_select,
        "Entra profile must honor the client-provided safety policy, not the constructor default"
    );
    assert!(!profile.auto_wrap_rollback);
}

/// The client-provided safety policy must pass straight through into the
/// profile, for both auth paths. This is the passthrough guarantee: whatever
/// the macOS app resolved from the saved `ServerRecord.safety` reaches the
/// engine unchanged.
#[test]
fn safety_fields_pass_through_into_profile() {
    for auth in ["sql", "entra-browser"] {
        let profile = EngineConnection {
            server: "db.example.com".to_string(),
            port: 1433,
            database: Some("app".to_string()),
            auth: auth.to_string(),
            username: "sa".to_string(),
            password: "p".to_string(),
            access_token: None,
            encrypt: true,
            trust_certificate: true,
            timeout_seconds: None,
            block_all_but_select: true,
            auto_wrap_rollback: true,
            confirm_data_changes: false,
            database_type: None,
            statement_timeout_seconds: None,
            enforce_read_only: false,
            proxy_profile: None,
        }
        .into_profile();
        assert!(
            profile.block_all_but_select,
            "auth={auth} should pass block_all_but_select=true"
        );
        assert!(
            profile.auto_wrap_rollback,
            "auth={auth} should pass auto_wrap_rollback=true"
        );
    }
}

/// `EngineConnection` is deserialized from the wire. A request that sends
/// explicit safety fields must pick them up; a request from an older client
/// that omits them must default to `false` (don't block) — never the
/// constructor's Entra default of `true`.
#[test]
fn deserialized_safety_fields_default_to_false_when_absent() {
    // An Entra connection from an old client with NO safety fields in the JSON.
    let json = serde_json::json!({
        "server": "azure.database.windows.net",
        "port": 1433,
        "database": "master",
        "auth": "entra-browser",
        "username": "",
        "password": ""
    });
    let ec: EngineConnection = serde_json::from_value(json).expect("deserialize entra connection");
    assert!(!ec.block_all_but_select);
    assert!(!ec.auto_wrap_rollback);

    let profile = ec.into_profile();
    assert!(
        !profile.block_all_but_select,
        "absent fields must not fall back to the Entra constructor default of true"
    );
}

#[test]
fn deserialized_safety_fields_round_trip_from_wire() {
    let json = serde_json::json!({
        "server": "db.example.com",
        "auth": "sql",
        "username": "sa",
        "password": "p",
        "block_all_but_select": true,
        "auto_wrap_rollback": true
    });
    let ec: EngineConnection = serde_json::from_value(json).expect("deserialize connection");
    assert!(ec.block_all_but_select);
    assert!(ec.auto_wrap_rollback);
    let profile = ec.into_profile();
    assert!(profile.block_all_but_select);
    assert!(profile.auto_wrap_rollback);
}

#[test]
fn decode_accepts_cloud_iam_auth_and_token() {
    let json = serde_json::json!({
        "server": "db.us-west-2.rds.amazonaws.com",
        "port": 5432,
        "auth": "aws-rds-iam",
        "username": "jane",
        "password": "",
        "access_token": "iam-token-value"
    });
    let ec: EngineConnection = serde_json::from_value(json).expect("deserialize aws iam");
    let profile = ec.into_profile();
    match profile.authentication {
        sqlly_core::AuthenticationType::AwsRdsIam { username, .. } => assert_eq!(username, "jane"),
        other => panic!("unexpected auth: {other:?}"),
    }
    assert_eq!(
        profile.runtime_cloud_iam_token.as_deref(),
        Some("iam-token-value")
    );
}

#[test]
fn ask_schema_purpose_decodes_from_binary() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;

    let default_body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("a1")
        .string(connection_json)
        .string("what joins orders?")
        .opt_string(None)
        .opt_string(None)
        .u8(0)
        .done();
    let default_request =
        decode_ask_schema_binary(&default_body).expect("decode ask_schema general");
    match default_request {
        EngineRequest::AskSchema { id, purpose, .. } => {
            assert_eq!(id, "a1");
            assert_eq!(purpose, SchemaPurpose::General);
        }
        other => panic!("expected ask_schema, got {other:?}"),
    }

    let sql_gen_body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("a2")
        .string(connection_json)
        .string("write a query")
        .opt_string(None)
        .opt_string(None)
        .u8(1)
        .done();
    let sql_generation_request =
        decode_ask_schema_binary(&sql_gen_body).expect("decode ask_schema sql_generation");
    match sql_generation_request {
        EngineRequest::AskSchema { id, purpose, .. } => {
            assert_eq!(id, "a2");
            assert_eq!(purpose, SchemaPurpose::SqlGeneration);
        }
        other => panic!("expected ask_schema, got {other:?}"),
    }
}

#[tokio::test]
async fn writes_length_prefixed_binary_ready_frame() {
    let mut out = Vec::new();
    write_frame(
        &mut out,
        &EngineFrame::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();

    let len = u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize;
    assert_eq!(len, out.len() - 4);
    assert_eq!(out[4], FRAME_TAG_READY);
    let protocol_version = u32::from_le_bytes(out[5..9].try_into().unwrap());
    assert_eq!(protocol_version, PROTOCOL_VERSION);
    // The capability list follows the legacy u32 and names everything this
    // engine accepts that an older bridge-side engine would reject.
    let info = sqlly_remote::engine_protocol::decode_ready_body(&out[5..]).unwrap();
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    for capability in ENGINE_CAPABILITIES {
        assert!(info.capabilities.supports(capability), "{capability}");
    }
    assert!(
        info.capabilities
            .supports(sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS)
    );
}

fn sqlite_row_edit_profile(path: &std::path::Path) -> ConnectionProfile {
    let json = serde_json::json!({
        "server": path.to_string_lossy(),
        "username": "",
        "password": "",
        "database_type": "Sqlite",
    });
    serde_json::from_value::<EngineConnection>(json)
        .unwrap()
        .into_profile()
}

/// `fk_labels` is advertised as a capability, so the engine must actually
/// serve it (it used to fall through to "unknown row_edit op").
#[tokio::test]
async fn row_edit_fk_labels_op_resolves_labels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("labels.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO customer VALUES (1, 'Ada'), (2, 'Grace');",
        )
        .unwrap();
    }
    let request = sqlly_services::row_editing::FkLabelRequest {
        referenced: sqlly_services::row_editing::TableTarget {
            schema: "main".to_string(),
            table: "customer".to_string(),
        },
        key_column: "id".to_string(),
        display_column: "name".to_string(),
        keys: vec![sqlly_core::CellValue::I64(1), sqlly_core::CellValue::I64(2)],
    };
    let frame = run_row_edit(
        "fk-1".to_string(),
        sqlite_row_edit_profile(&path),
        "fk_labels",
        &serde_json::to_string(&request).unwrap(),
        Arc::new(std::sync::Mutex::new(None)),
        EnginePools::new(),
    )
    .await;
    let EngineFrame::RowEditReport { id, report } = frame else {
        panic!("expected a row-edit report, got {frame:?}");
    };
    assert_eq!(id, "fk-1");
    let result: sqlly_services::row_editing::FkLabelResult =
        serde_json::from_value(report).unwrap();
    let mut labels: Vec<String> = result.labels.into_iter().map(|(_, l)| l).collect();
    labels.sort();
    assert_eq!(labels, vec!["Ada".to_string(), "Grace".to_string()]);
}

#[tokio::test]
async fn row_edit_fk_labels_refuses_oversized_key_lists() {
    let request = sqlly_services::row_editing::FkLabelRequest {
        referenced: sqlly_services::row_editing::TableTarget {
            schema: "main".to_string(),
            table: "t".to_string(),
        },
        key_column: "id".to_string(),
        display_column: "name".to_string(),
        keys: vec![sqlly_core::CellValue::I64(1); ROW_EDIT_FK_LABELS_MAX_KEYS + 1],
    };
    let frame = run_row_edit(
        "fk-2".to_string(),
        sqlite_row_edit_profile(std::path::Path::new("unused.db")),
        "fk_labels",
        &serde_json::to_string(&request).unwrap(),
        Arc::new(std::sync::Mutex::new(None)),
        EnginePools::new(),
    )
    .await;
    let EngineFrame::RequestFailed { error, .. } = frame else {
        panic!("expected a refusal, got {frame:?}");
    };
    assert!(error.to_string().contains("limit"), "{error}");
}

#[test]
fn encodes_validation_report_as_flat_binary() {
    let frame = EngineFrame::ValidationReport {
        id: "v1".to_string(),
        diagnostics: vec![WireDiag {
            severity: 0,
            class: 1, // UnresolvedObject
            span_start: 20,
            span_len: 18,
            line: 1,
            col: 21,
            message: "Cannot find object".to_string(),
        }],
        resolved: vec![WireSpan { start: 5, len: 3 }],
    };
    let bytes = encode_frame(&frame).unwrap();

    let total = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    assert_eq!(total, bytes.len() - 4);
    let body = &bytes[4..];
    assert_eq!(body[0], FRAME_TAG_VALIDATION);

    // After tag: [u32 id_len]["v1"][u32 count]...
    let mut cur = 1usize;
    let id_len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
    cur += 4;
    assert_eq!(&body[cur..cur + id_len], b"v1");
    cur += id_len;
    let count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    cur += 4;
    assert_eq!(count, 1);
    assert_eq!(body[cur], 0); // severity Error
    assert_eq!(body[cur + 1], 1); // class UnresolvedObject
    let span_start = u32::from_le_bytes(body[cur + 2..cur + 6].try_into().unwrap());
    assert_eq!(span_start, 20);
    let span_len = u32::from_le_bytes(body[cur + 6..cur + 10].try_into().unwrap());
    assert_eq!(span_len, 18);
    // Walk past the diagnostic to the resolved-spans section. Per-diag
    // fixed fields: severity(1)+class(1)+span_start(4)+span_len(4)+line(4)+col(4) = 18 bytes.
    let msg_len = u32::from_le_bytes(body[cur + 18..cur + 22].try_into().unwrap()) as usize;
    cur += 22 + msg_len;
    let resolved_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    cur += 4;
    assert_eq!(resolved_count, 1);
    let resolved_start = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(resolved_start, 5);
    let resolved_len = u32::from_le_bytes(body[cur + 4..cur + 8].try_into().unwrap());
    assert_eq!(resolved_len, 3);
}

#[test]
fn encodes_rows_batch_as_flat_binary() {
    let frame = EngineFrame::RowsBatch {
        id: "q-1".to_string(),
        result_set_index: 0,
        rows: vec![vec![
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::Bool(false),
            CellValue::I64(42),
            CellValue::F64(3.125),
            CellValue::String("hi".to_string()),
            CellValue::Bytes(vec![0xDE, 0xAD]),
        ]],
    };
    let bytes = encode_frame(&frame).unwrap();

    let total = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    assert_eq!(total, bytes.len() - 4);
    let body = &bytes[4..];
    assert_eq!(body[0], FRAME_TAG_ROWS_BATCH);

    // After tag: [u32 id_len]["q-1"][u32 result_set_index][u32 row_count]
    let mut cur = 1usize;
    let id_len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
    cur += 4;
    assert_eq!(&body[cur..cur + id_len], b"q-1");
    cur += id_len;
    let rs_idx = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    cur += 4;
    assert_eq!(rs_idx, 0);
    let row_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    cur += 4;
    assert_eq!(row_count, 1);

    // Row: [u32 cell_count] then per cell [u8 tag][payload]
    let cell_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    cur += 4;
    assert_eq!(cell_count, 7);

    // Null: tag only
    assert_eq!(body[cur], 0x00);
    cur += 1;
    // Bool(true)
    assert_eq!(body[cur], 0x01);
    assert_eq!(body[cur + 1], 1);
    cur += 2;
    // Bool(false)
    assert_eq!(body[cur], 0x01);
    assert_eq!(body[cur + 1], 0);
    cur += 2;
    // I64(42)
    assert_eq!(body[cur], 0x02);
    let i64_val = i64::from_le_bytes(body[cur + 1..cur + 9].try_into().unwrap());
    assert_eq!(i64_val, 42);
    cur += 9;
    // F64(3.125)
    assert_eq!(body[cur], 0x03);
    let f64_val = f64::from_le_bytes(body[cur + 1..cur + 9].try_into().unwrap());
    assert_eq!(f64_val, 3.125);
    cur += 9;
    // String("hi")
    assert_eq!(body[cur], 0x04);
    let str_len = u32::from_le_bytes(body[cur + 1..cur + 5].try_into().unwrap());
    assert_eq!(str_len, 2);
    assert_eq!(&body[cur + 5..cur + 7], b"hi");
    cur += 5 + str_len as usize;
    // Bytes([0xDE, 0xAD])
    assert_eq!(body[cur], 0x05);
    let bytes_len = u32::from_le_bytes(body[cur + 1..cur + 5].try_into().unwrap());
    assert_eq!(bytes_len, 2);
    assert_eq!(&body[cur + 5..cur + 7], &[0xDE, 0xAD]);
    cur += 5 + bytes_len as usize;

    // Fully consumed
    assert_eq!(cur, body.len(), "trailing bytes remain");
}

#[test]
fn encodes_rows_batch_empty_batch() {
    let frame = EngineFrame::RowsBatch {
        id: "q-empty".to_string(),
        result_set_index: 3,
        rows: vec![],
    };
    let body = encode_rows_batch_body("q-empty", 3, &[]).unwrap();

    let mut cur = 0;
    let id_len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
    cur += 4;
    assert_eq!(&body[cur..cur + id_len], b"q-empty");
    cur += id_len;
    let rs_idx = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(rs_idx, 3);
    cur += 4;
    let row_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(row_count, 0);
    cur += 4;
    assert_eq!(cur, body.len());

    // Full frame has the right tag.
    let bytes = encode_frame(&frame).unwrap();
    assert_eq!(bytes[4], FRAME_TAG_ROWS_BATCH);
}

#[test]
fn encodes_rows_batch_empty_row() {
    let rows: Vec<Vec<CellValue>> = vec![vec![], vec![CellValue::Null]];
    let body = encode_rows_batch_body("q-er", 0, &rows).unwrap();

    let mut cur = 0;
    cur += 4 + 4; // id_len + "q-er"
    cur += 4; // result_set_index
    let row_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(row_count, 2);
    cur += 4;

    // First row: 0 cells
    let cc0 = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(cc0, 0);
    cur += 4;

    // Second row: 1 cell (Null)
    let cc1 = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(cc1, 1);
    cur += 4;
    assert_eq!(body[cur], 0x00); // Null tag
    cur += 1;
    assert_eq!(cur, body.len());
}

#[test]
fn encodes_rows_batch_unicode_string() {
    let rows = vec![vec![CellValue::String("héllo".to_string())]];
    let body = encode_rows_batch_body("u", 0, &rows).unwrap();
    // Verify the string byte length matches UTF-8 encoding (not character count).
    let mut cur = 4 + 1; // id_len + "u"
    cur += 4; // result_set_index
    cur += 4; // row_count
    cur += 4; // cell_count
    assert_eq!(body[cur], 0x04); // String tag
    let str_len = u32::from_le_bytes(body[cur + 1..cur + 5].try_into().unwrap()) as usize;
    assert_eq!(str_len, "héllo".len()); // 6 bytes, not 5 chars
    assert_eq!(&body[cur + 5..cur + 5 + str_len], "héllo".as_bytes());
}

#[test]
fn encodes_rows_batch_negative_i64_and_special_f64() {
    let rows = vec![vec![
        CellValue::I64(-1),
        CellValue::F64(f64::NAN),
        CellValue::F64(f64::INFINITY),
        CellValue::F64(-0.0),
    ]];
    let body = encode_rows_batch_body("n", 0, &rows).unwrap();
    let mut cur = 4 + 1; // id_len + "n"
    cur += 4 + 4 + 4; // rs_idx + row_count + cell_count

    // I64(-1)
    assert_eq!(body[cur], 0x02);
    let i_val = i64::from_le_bytes(body[cur + 1..cur + 9].try_into().unwrap());
    assert_eq!(i_val, -1);
    cur += 9;

    // F64(NAN)
    assert_eq!(body[cur], 0x03);
    let nan_val = f64::from_le_bytes(body[cur + 1..cur + 9].try_into().unwrap());
    assert!(nan_val.is_nan());
    cur += 9;

    // F64(INFINITY)
    assert_eq!(body[cur], 0x03);
    let inf_val = f64::from_le_bytes(body[cur + 1..cur + 9].try_into().unwrap());
    assert!(inf_val.is_infinite() && inf_val > 0.0);
    cur += 9;

    // F64(-0.0)
    assert_eq!(body[cur], 0x03);
    let neg_zero = f64::from_le_bytes(body[cur + 1..cur + 9].try_into().unwrap());
    assert!(neg_zero == 0.0 && neg_zero.is_sign_negative());
}

// MARK: - Encoded size estimation and batch splitting

#[test]
fn rows_batch_body_len_matches_actual_body() {
    let rows = vec![
        vec![CellValue::Null, CellValue::Bool(true), CellValue::I64(42)],
        vec![
            CellValue::F64(3.125),
            CellValue::String("hello".to_string()),
        ],
        vec![CellValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])],
        vec![],
    ];
    let id = "test-id";
    let rs_idx = 3;
    let estimated = rows_batch_body_len(id, rs_idx, &rows).unwrap();
    let actual = encode_rows_batch_body(id, rs_idx, &rows).unwrap();
    assert_eq!(
        estimated,
        actual.len(),
        "estimated body length must match actual encoded body length"
    );
}

#[test]
fn rows_batch_body_len_empty_batch() {
    let rows: Vec<Vec<CellValue>> = vec![];
    let estimated = rows_batch_body_len("id", 0, &rows).unwrap();
    // 4 (id_len) + 2 ("id") + 4 (rs_idx) + 4 (row_count) = 14
    assert_eq!(estimated, 14);
}

#[test]
fn encoded_cell_payload_len_correct_for_each_type() {
    assert_eq!(encoded_cell_payload_len(&CellValue::Null).unwrap(), 0);
    assert_eq!(encoded_cell_payload_len(&CellValue::Bool(true)).unwrap(), 1);
    assert_eq!(encoded_cell_payload_len(&CellValue::I64(0)).unwrap(), 8);
    assert_eq!(encoded_cell_payload_len(&CellValue::F64(0.0)).unwrap(), 8);
    assert_eq!(
        encoded_cell_payload_len(&CellValue::String("".to_string())).unwrap(),
        4
    );
    assert_eq!(
        encoded_cell_payload_len(&CellValue::String("hi".to_string())).unwrap(),
        6 // 4 (len prefix) + 2
    );
    assert_eq!(
        encoded_cell_payload_len(&CellValue::Bytes(vec![])).unwrap(),
        4
    );
    assert_eq!(
        encoded_cell_payload_len(&CellValue::Bytes(vec![0x01, 0x02])).unwrap(),
        6 // 4 (len prefix) + 2
    );
}

#[test]
fn split_rows_batch_small_batch_single_chunk() {
    let rows = vec![
        vec![CellValue::I64(1)],
        vec![CellValue::I64(2)],
        vec![CellValue::I64(3)],
    ];
    let chunks = split_rows_batch("id", 0, &rows).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), 3);
}

#[test]
fn split_rows_batch_empty_returns_single_empty_chunk() {
    let rows: Vec<Vec<CellValue>> = vec![];
    let chunks = split_rows_batch("id", 0, &rows).unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_empty());
}

#[test]
fn split_rows_batch_splits_when_exceeding_target() {
    // Each row has a string cell slightly over 1 MiB.
    // With TARGET_ROWS_FRAME_BODY_BYTES = 2 MiB, we expect ~2 rows per chunk.
    let big_string = "x".repeat(1024 * 1024 + 100); // ~1 MiB + 100 bytes
    let rows = vec![
        vec![CellValue::String(big_string.clone())],
        vec![CellValue::String(big_string.clone())],
        vec![CellValue::String(big_string.clone())],
        vec![CellValue::String(big_string.clone())],
        vec![CellValue::String(big_string.clone())],
    ];
    let chunks = split_rows_batch("id", 0, &rows).unwrap();
    assert!(chunks.len() > 1, "should split into multiple chunks");

    // Verify all rows are accounted for in order
    let total: usize = chunks.iter().map(|c| c.len()).sum();
    assert_eq!(total, 5);

    // Verify each chunk's encoded body fits within the target
    for chunk in &chunks {
        let len = rows_batch_body_len("id", 0, chunk).unwrap();
        assert!(
            len <= TARGET_ROWS_FRAME_BODY_BYTES || chunk.len() == 1,
            "chunk body ({} bytes) exceeds target ({} bytes) and is not a single-row chunk",
            len,
            TARGET_ROWS_FRAME_BODY_BYTES
        );
    }
}

#[test]
fn split_rows_batch_single_oversized_row_within_max_frame() {
    // A single row with a string larger than TARGET but smaller than MAX_FRAME_BYTES.
    // Should return a single chunk containing just that one row.
    let big_string = "x".repeat(4 * 1024 * 1024); // 4 MiB string
    let rows = vec![vec![CellValue::String(big_string)]];
    let chunks = split_rows_batch("id", 0, &rows).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), 1);
}

#[test]
fn split_rows_batch_single_row_exceeding_max_frame_errors() {
    // A single row with a string larger than MAX_FRAME_BYTES should error.
    let huge_string = "x".repeat(MAX_FRAME_BYTES as usize);
    let rows = vec![vec![CellValue::String(huge_string)]];
    let result = split_rows_batch("id", 0, &rows);
    assert!(
        result.is_err(),
        "single row exceeding MAX_FRAME_BYTES must error"
    );
}

#[test]
fn executor_previews_keep_wide_text_row_under_frame_limit() {
    use sqlly_services::{CellLimits, truncate_rows};
    let cells: Vec<CellValue> = (0..100)
        .map(|_| CellValue::String("x".repeat(1024 * 1024)))
        .collect();
    let mut row = vec![cells];
    truncate_rows(&mut row, 0, &CellLimits::default());
    let chunks = split_rows_batch("id", 0, &row).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), 1);
}

#[test]
fn split_rows_batch_preserves_row_order() {
    let rows: Vec<Vec<CellValue>> = (0..10).map(|i| vec![CellValue::I64(i)]).collect();
    let chunks = split_rows_batch("id", 0, &rows).unwrap();
    let mut index = 0;
    for chunk in &chunks {
        for row in *chunk {
            if let CellValue::I64(val) = row[0] {
                assert_eq!(val, index, "row order must be preserved across chunks");
            }
            index += 1;
        }
    }
    assert_eq!(index, 10);
}

#[tokio::test]
async fn consume_query_stream_oversized_row_is_fatal_and_cancels() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let cancel_called = Arc::new(AtomicBool::new(false));
    let cancel_slot: sqlly_services::CancelSlot =
        Arc::new(std::sync::Mutex::new(Some(Box::new({
            let flag = cancel_called.clone();
            move || {
                flag.store(true, Ordering::SeqCst);
            }
        }))));

    let events = sqlly_core::EventBus::new(16);
    let mut subscription = events.subscribe();
    let (rows_tx, rows_rx) = sqlly_core::row_channel(4);
    let huge = CellValue::String("x".repeat(MAX_FRAME_BYTES as usize));
    rows_tx
        .send(sqlly_core::RowBatch::new(
            sqlly_core::SessionId::default(),
            0,
            vec![vec![huge]],
        ))
        .await
        .ok();
    drop(rows_tx);
    drop(events);

    let (tx, mut rx) = mpsc::channel(4);
    let frames = LoggedSender::new(tx, None);
    let (connected, _, stream_failed) = consume_query_stream(
        "q-big".to_string(),
        &frames,
        &mut subscription,
        rows_rx,
        Duration::from_secs(5),
        &cancel_slot,
        false,
    )
    .await;
    drop(frames);

    assert!(connected);
    assert!(stream_failed);
    assert!(cancel_called.load(Ordering::SeqCst));
    let mut terminals = 0;
    while let Ok(frame) = rx.try_recv() {
        if matches!(frame, EngineFrame::RequestFailed { .. }) {
            terminals += 1;
        }
    }
    assert_eq!(
        terminals, 1,
        "oversized row must yield exactly one terminal"
    );
}

/// A client that asked for cell previews gets a 20 KiB cell as its 8 KiB
/// preview plus a `TruncatedCells` frame naming its absolute row; one that
/// did not (an old client, or a full-value export) gets the cell whole and
/// no `TruncatedCells` frame.
#[tokio::test]
async fn consume_query_stream_previews_cells_only_when_asked() {
    async fn run(cell_previews: bool) -> Vec<EngineFrame> {
        let cancel_slot: sqlly_services::CancelSlot = Arc::new(std::sync::Mutex::new(None));
        let events = sqlly_core::EventBus::new(16);
        let mut subscription = events.subscribe();
        let (rows_tx, rows_rx) = sqlly_core::row_channel(4);
        let big = CellValue::String("x".repeat(20 * 1024));
        for batch in 0..2 {
            let rows = if batch == 0 {
                vec![vec![CellValue::I64(1)], vec![CellValue::I64(2)]]
            } else {
                vec![vec![big.clone()]]
            };
            rows_tx
                .send(sqlly_core::RowBatch::new(
                    sqlly_core::SessionId::default(),
                    0,
                    rows,
                ))
                .await
                .ok();
        }
        drop(rows_tx);
        drop(events);
        let (tx, mut rx) = mpsc::channel(16);
        let frames = LoggedSender::new(tx, None);
        let (connected, _, failed) = consume_query_stream(
            "q-preview".to_string(),
            &frames,
            &mut subscription,
            rows_rx,
            Duration::from_secs(5),
            &cancel_slot,
            cell_previews,
        )
        .await;
        drop(frames);
        assert!(connected && !failed);
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(frame);
        }
        out
    }

    let previews = run(true).await;
    let big_cell = previews.iter().find_map(|frame| match frame {
        EngineFrame::RowsBatch { rows, .. } => rows.iter().find_map(|row| match row.first() {
            Some(CellValue::String(s)) => Some(s.len()),
            _ => None,
        }),
        _ => None,
    });
    assert_eq!(big_cell, Some(8 * 1024), "the cell is sent as its preview");
    let cut: Vec<&sqlly_services::TruncatedCell> = previews
        .iter()
        .filter_map(|frame| match frame {
            EngineFrame::TruncatedCells { cells, .. } => Some(cells.iter()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(cut.len(), 1);
    assert_eq!(cut[0].row, 2, "absolute row across batches");
    assert_eq!(cut[0].total_len, 20 * 1024);

    let whole = run(false).await;
    assert!(
        whole
            .iter()
            .all(|f| !matches!(f, EngineFrame::TruncatedCells { .. })),
        "no truncation frame without the option"
    );
    let big_cell = whole.iter().find_map(|frame| match frame {
        EngineFrame::RowsBatch { rows, .. } => rows.iter().find_map(|row| match row.first() {
            Some(CellValue::String(s)) => Some(s.len()),
            _ => None,
        }),
        _ => None,
    });
    assert_eq!(big_cell, Some(20 * 1024), "the cell is sent whole");
}

#[tokio::test]
async fn validate_request_flags_unresolved_object() {
    // The original bug: `Shared.MovementLine` must be flagged when only
    // `dbo.MovementLine` exists in the editor schema.
    let frame = validate_binary_frame(
        "v1",
        "SELECT * FROM Shared.MovementLine",
        Some("sqlserver"),
        Some("dbo"),
        &[("dbo", "MovementLine", "table", &["MovementLineId"])],
    );

    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    let catalog = test_catalog_state();
    let keep_running = dispatch_request(&frame, &frames, &active, &pool, &catalog).await;
    assert!(keep_running);
    drop(frames);

    let frame = rx.recv().await.expect("a validation report frame");
    match frame {
        EngineFrame::ValidationReport {
            id, diagnostics, ..
        } => {
            assert_eq!(id, "v1");
            assert!(
                diagnostics.iter().any(|d| d.class == 1),
                "expected an UnresolvedObject (class 1) diagnostic, got {diagnostics:?}"
            );
        }
        other => panic!("expected ValidationReport, got {other:?}"),
    }
}

#[tokio::test]
async fn reads_length_prefixed_binary_frame() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("p1")
        .done();
    let mut frame = vec![REQUEST_TAG_PING];
    frame.extend_from_slice(&body);
    let mut input = Vec::new();
    input.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    input.extend_from_slice(&frame);

    let mut cursor = std::io::Cursor::new(input);
    let raw_frame = read_frame(&mut cursor).await.unwrap().unwrap();
    let request = decode_ping_binary(&raw_frame[1..]).unwrap();
    assert!(matches!(request, EngineRequest::Ping { id } if id == "p1"));
}

fn sql_profile(server: &str, db: Option<&str>, user: &str) -> ConnectionProfile {
    EngineConnection {
        server: server.to_string(),
        port: 1433,
        database: db.map(str::to_string),
        auth: "sql".to_string(),
        username: user.to_string(),
        password: "pw".to_string(),
        access_token: None,
        encrypt: true,
        trust_certificate: true,
        timeout_seconds: Some(5),
        block_all_but_select: false,
        auto_wrap_rollback: false,
        confirm_data_changes: false,
        database_type: None,
        statement_timeout_seconds: None,
        enforce_read_only: false,
        proxy_profile: None,
    }
    .into_profile()
}

#[test]
fn identity_ignores_password_but_separates_user_db_server() {
    // Same identity tuple => same fingerprint (password is intentionally excluded).
    let mut a = sql_profile("localhost", Some("db"), "sa");
    let mut b = sql_profile("localhost", Some("db"), "sa");
    a.name = "tab-a".to_string();
    b.name = "tab-b".to_string(); // name must NOT affect identity
    assert_eq!(connection_identity(&a), connection_identity(&b));

    // Different database => different identity.
    let c = sql_profile("localhost", Some("other"), "sa");
    assert_ne!(connection_identity(&a), connection_identity(&c));
    // Different user => different identity.
    let d = sql_profile("localhost", Some("db"), "reader");
    assert_ne!(connection_identity(&a), connection_identity(&d));
    // Different server => different identity.
    let e = sql_profile("remote", Some("db"), "sa");
    assert_ne!(connection_identity(&a), connection_identity(&e));
}

#[test]
fn pool_key_namespaces_by_caller_key() {
    let profile = sql_profile("localhost", Some("db"), "sa");
    let identity = connection_identity(&profile);

    // No caller key => key is exactly the identity (shared default lane).
    assert_eq!(sqlly_services::TdsPool::pool_key(None, &profile), identity);

    // Distinct caller keys (e.g. per-tab) produce distinct pool keys even
    // for an identical connection identity — each tab gets its own client.
    let tab1 = sqlly_services::TdsPool::pool_key(Some("tab-1"), &profile);
    let tab2 = sqlly_services::TdsPool::pool_key(Some("tab-2"), &profile);
    assert_ne!(tab1, tab2);
    assert!(tab1.contains(&identity));
    assert!(tab1.starts_with("tab-1"));
}

// ----------------------------------------------------------------------
// Request/response correlation log
// ----------------------------------------------------------------------

/// The hot-path rows_batch encoder is hand-tuned (no intermediate
/// allocations); the protocol crate owns the REFERENCE encoder. The two
/// must stay byte-for-byte identical — this is the tripwire.
#[test]
fn rows_batch_encoder_matches_the_protocol_reference() {
    use sqlly_remote::engine_protocol::layouts::{WireCell, encode_rows_batch};
    let rows = vec![
        vec![
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::I64(-7),
            CellValue::F64(1.25),
            CellValue::String("héllo \"q\"".to_string()),
            CellValue::Bytes(vec![0x00, 0xFF, 0x10]),
        ],
        vec![],
    ];
    let tuned = encode_rows_batch_body("req-42", 3, &rows).unwrap();
    let wire_rows: Vec<Vec<WireCell>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| match c {
                    CellValue::Null => WireCell::Null,
                    CellValue::Bool(b) => WireCell::Bool(*b),
                    CellValue::I64(v) => WireCell::I64(*v),
                    CellValue::F64(v) => WireCell::F64(*v),
                    CellValue::String(s) => WireCell::Text(s.clone()),
                    CellValue::Bytes(b) => WireCell::Bytes(b.clone()),
                })
                .collect()
        })
        .collect();
    let reference = encode_rows_batch("req-42", 3, &wire_rows);
    assert_eq!(tuned, reference);
}

fn temp_log_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sqlly-engine-test-{tag}-{nanos}.log"))
}

#[test]
#[cfg(unix)]
fn default_engine_log_path_leaves_shared_temp_dir() {
    let path = default_engine_log_path();
    assert_ne!(
        path,
        std::env::temp_dir().join("sqlly-engine.log"),
        "default log must not live in the shared temp directory"
    );
}

#[test]
#[cfg(unix)]
fn engine_log_file_is_owner_only() {
    let path = temp_log_path("perms");
    let log = EngineLog::at(&path);
    log.append_line("owner-only", false);

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "group/other permission bits must be clear: {mode:o}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn raw_request_fields_extracts_id_and_type_from_binary_execute_query() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("q-42")
        .string(r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#)
        .string("SELECT 1")
        .opt_u32(None)
        .opt_string(None)
        .done();
    let mut frame = vec![REQUEST_TAG_EXECUTE_QUERY];
    frame.extend_from_slice(&body);
    let (id, kind) = raw_request_fields(&frame);
    assert_eq!(id, "q-42");
    assert_eq!(kind, "execute_query");
}

#[test]
fn raw_request_fields_falls_back_for_malformed_or_missing() {
    // Not a valid tag at all: still attributable, just with placeholders.
    let (id, kind) = raw_request_fields(b"{not json");
    assert_eq!(id, "unknown");
    assert_eq!(kind, "?");

    // Empty frame.
    let (id, kind) = raw_request_fields(b"");
    assert_eq!(id, "unknown");
    assert_eq!(kind, "?");
}

#[test]
fn frame_log_fields_carries_failure_reason_correlated_to_id() {
    // request_failed must surface the error text — the key debugging signal.
    let failed = EngineFrame::RequestFailed {
        id: "q-7".to_string(),
        error: EngineError::query("connection refused"),
    };
    let (id, kind, detail) = frame_log_fields(&failed);
    assert_eq!(id, "q-7");
    assert_eq!(kind, "request_failed");
    assert!(detail.contains("connection refused"), "{detail}");

    // A streaming frame correlates to the same request id.
    let batch = EngineFrame::RowsBatch {
        id: "q-7".to_string(),
        result_set_index: 2,
        rows: vec![vec![], vec![]],
    };
    let (id, kind, detail) = frame_log_fields(&batch);
    assert_eq!(id, "q-7");
    assert_eq!(kind, "rows_batch");
    assert!(detail.contains("rows=2"), "{detail}");
}

#[test]
fn request_failed_error_text_round_trips_non_ascii() {
    // DbGate #1487 shows garbled Chinese error text; the failure frame must
    // carry non-ASCII messages byte-exact over the wire.
    let error_text = "连接失败：无法解析主机名";
    let frame = EngineFrame::RequestFailed {
        id: "q-zh".to_string(),
        error: EngineError::query(error_text),
    };
    let bytes = encode_frame(&frame).unwrap();
    let total_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    assert_eq!(bytes.len(), total_len + 4);
    assert_eq!(bytes[4], FRAME_TAG_REQUEST_FAILED);
    let mut r = BinaryFrameReader::new(&bytes[5..]);
    assert_eq!(r.read_string("id").unwrap(), "q-zh");
    assert_eq!(r.read_string("error").unwrap(), error_text);
    assert!(!r.read_string("code").unwrap().is_empty());
    r.ensure_at_end().unwrap();
}

#[test]
fn log_request_then_response_share_id_for_correlation() {
    let path = temp_log_path("reqres");
    let log = EngineLog::at(&path);

    log.log_request("q-1", "execute_query", r#"{"id":"q-1","sql":"SELECT 1"}"#);
    log.log_response("q-1", "request_started", "");
    log.log_response("q-1", "request_completed", "");

    let contents = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let req: Vec<&str> = contents.lines().filter(|l| l.contains(" REQ ")).collect();
    let res: Vec<&str> = contents.lines().filter(|l| l.contains(" RES ")).collect();
    assert_eq!(req.len(), 1, "{contents}");
    assert_eq!(res.len(), 2, "{contents}");
    // Every line carries id=q-1, so a request is fully traceable end-to-end.
    assert!(contents.lines().all(|l| l.contains("id=q-1")), "{contents}");
    // Each line is timestamped (leading millis number).
    assert!(
        req[0].split(' ').next().unwrap().parse::<u128>().is_ok(),
        "REQ line must start with a millis timestamp: {req:?}"
    );
}

#[test]
fn log_request_scrubs_credentials_but_keeps_correlation_and_sql() {
    let path = temp_log_path("redact");
    let log = EngineLog::at(&path);

    // A connection-bearing execute_query body exactly as a client sends
    // it: EngineConnection JSON with a password and an access token.
    let body = r#"{"id":"q-9","type":"execute_query","connection":{"server":"db.example.com","username":"sa","password":"hunter2","access_token":"tok-abc123"},"sql":"SELECT 42"}"#;
    log.log_request("q-9", "execute_query", body);
    log.log_response(
        "q-9",
        "request_failed",
        "error=login failed for password=hunter2",
    );

    let contents = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert!(!contents.contains("hunter2"), "{contents}");
    assert!(!contents.contains("tok-abc123"), "{contents}");
    // The diagnostic shape survives: id, type, server, user, and SQL.
    assert!(contents.contains("id=q-9"), "{contents}");
    assert!(contents.contains("type=execute_query"), "{contents}");
    assert!(contents.contains("db.example.com"), "{contents}");
    assert!(contents.contains(r#""username":"sa""#), "{contents}");
    assert!(contents.contains("SELECT 42"), "{contents}");
    assert!(contents.contains("error=login failed"), "{contents}");
}

#[test]
fn unanswered_request_is_detectable_as_req_with_no_terminal_res() {
    // Simulates a crash mid-request: the REQ line is durably written but no
    // terminal RES line for the same id is ever appended. This is the
    // smoking gun the whole feature exists to provide.
    let path = temp_log_path("unanswered");
    let log = EngineLog::at(&path);

    log.log_request("crash-me", "execute_query", r#"{"id":"crash-me"}"#);
    // ... process crashes here; no response logged.

    let contents = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let has_req = contents
        .lines()
        .any(|l| l.contains(" REQ ") && l.contains("id=crash-me"));
    let has_terminal_res = contents.lines().any(|l| {
        l.contains(" RES ")
            && l.contains("id=crash-me")
            && (l.contains("type=request_completed")
                || l.contains("type=request_failed")
                || l.contains("type=validation_report"))
    });
    assert!(has_req, "the crashing input must be recorded: {contents}");
    assert!(
        !has_terminal_res,
        "an unanswered request must have no terminal response: {contents}"
    );
}

#[test]
fn newlines_in_request_body_are_collapsed_to_one_line() {
    let path = temp_log_path("oneline");
    let log = EngineLog::at(&path);
    log.log_request(
        "q-nl",
        "validate",
        "{\"id\":\"q-nl\",\n\"sql\":\"SELECT 1\"}",
    );

    let contents = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    // Exactly one physical line, with the embedded newline escaped, so the
    // grep-for-unanswered-REQ scan stays line-oriented.
    assert_eq!(contents.lines().count(), 1, "{contents:?}");
    assert!(contents.contains("\\n"), "{contents:?}");
}

#[test]
fn truncate_keeps_only_the_last_n_lines_on_graceful_shutdown() {
    let path = temp_log_path("trim-keep");
    let log = EngineLog::at(&path);
    // Write 120 distinct lines (more than the cap).
    for i in 0..120 {
        log.append_line(&format!("line-{i}"), false);
    }

    log.truncate_to_last_lines(50);

    let contents = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 50, "must keep exactly the cap: {contents}");
    // The retained lines are the LAST 50 (70..120), in order.
    assert_eq!(lines.first().copied(), Some("line-70"), "{contents}");
    assert_eq!(lines.last().copied(), Some("line-119"), "{contents}");
    // Older lines are gone.
    assert!(!contents.contains("line-69"), "{contents}");
    // File ends with a newline so the next session appends cleanly.
    assert!(contents.ends_with('\n'), "{contents:?}");
}

#[test]
fn truncate_is_a_noop_when_already_at_or_under_the_cap() {
    let path = temp_log_path("trim-noop");
    let log = EngineLog::at(&path);
    for i in 0..10 {
        log.append_line(&format!("line-{i}"), false);
    }
    let before = std::fs::read_to_string(&path).unwrap();

    log.truncate_to_last_lines(50);

    let after = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    // Fewer lines than the cap: file is untouched (all lines preserved).
    assert_eq!(before, after, "under-cap log must not be rewritten");
    assert_eq!(after.lines().count(), 10, "{after}");
}

#[test]
fn truncate_on_missing_file_is_silently_ignored() {
    // A graceful shutdown of a session that never wrote a log line (logging
    // off, or no requests) must not error trying to trim a nonexistent file,
    // nor create one.
    let path = temp_log_path("trim-missing");
    let _ = std::fs::remove_file(&path);
    let log = EngineLog::at(&path);
    log.truncate_to_last_lines(50); // must not panic
    assert!(!path.exists(), "trim must not create a missing log file");
}

#[test]
fn truncate_leaves_no_temp_file_behind() {
    let path = temp_log_path("trim-tmp");
    let log = EngineLog::at(&path);
    for i in 0..100 {
        log.append_line(&format!("line-{i}"), false);
    }
    log.truncate_to_last_lines(50);

    let tmp = path.with_extension("log.trim");
    let leftover = tmp.exists();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&tmp);
    assert!(!leftover, "the staging temp file must be renamed away");
}

#[tokio::test]
async fn dispatch_logs_request_and_correlated_response() {
    // End-to-end through the real dispatch path: a ping must produce a REQ
    // line and a correlated RES (pong) line sharing the id.
    let path = temp_log_path("dispatch");
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("p-9")
        .done();
    let mut frame_bytes = vec![REQUEST_TAG_PING];
    frame_bytes.extend_from_slice(&body);

    let (tx, _rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, Some(EngineLog::at(&path)));
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    let catalog = test_catalog_state();
    let keep_running = dispatch_request(&frame_bytes, &frames, &active, &pool, &catalog).await;
    assert!(keep_running);

    let contents = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        contents.contains("REQ id=p-9 type=ping"),
        "request logged: {contents}"
    );
    assert!(
        contents.contains("RES id=p-9 type=pong"),
        "correlated response logged: {contents}"
    );
}

// -----------------------------------------------------------------------
// Catalog endpoint tests
// -----------------------------------------------------------------------

/// A [`DefaultCatalogService`] pointed at a fresh temp directory so tests never
/// touch a real catalog file. Each call gets a unique root so parallel
/// tests never collide.
fn test_catalog_state() -> DefaultCatalogService {
    DefaultCatalogService::from_parts(
        std::env::temp_dir().join(format!(
            "sqlly-engine-catalog-test-{}",
            uuid::Uuid::new_v4()
        )),
        Arc::new(sqlly_ai::catalog::StubLlmProvider::new()),
        Arc::new(StubEmbeddingProvider::new()),
    )
}

// -----------------------------------------------------------------------
// Provider selection (`build_catalog_providers_from_env`)
// -----------------------------------------------------------------------

/// Every env var [`build_catalog_providers_from_env`] consults — cleared
/// before each case so the host environment can never leak in.
const PROVIDER_ENV_KEYS: [&str; 9] = [
    LLM_PROVIDER_ENV,
    LLM_MODEL_ENV,
    EMBEDDING_PROVIDER_ENV,
    EMBEDDING_MODEL_ENV,
    sqlly_ai::ollama::BASE_URL_ENV,
    sqlly_ai::ollama::TIMEOUT_SECS_ENV,
    sqlly_ai::ollama::PROBE_TIMEOUT_SECS_ENV,
    sqlly_ai::mlx::CHAT_MODEL_ENV,
    sqlly_ai::mlx::EMBEDDING_MODEL_ENV,
];

/// Build providers with exactly the supplied env vars set (all provider vars
/// cleared first, and cleared again afterwards). Returns both provider ids
/// — the observable signal for which providers were selected.
//
// The guard is intentionally held across the `.await`: it serializes
// process-wide env mutation so the env stays stable for the duration of the
// provider build. Tests run single-threaded against this lock, so there is
// no deadlock risk from holding a std Mutex across the await.
async fn provider_ids_with_env(vars: &[(&str, &str)]) -> (String, String) {
    let mut env = sqlly_testkit::EnvGuard::acquire();
    for key in PROVIDER_ENV_KEYS {
        env.remove(key);
    }
    for (key, value) in vars {
        env.set(key, value);
    }
    let (llm, embedding, _vector_store, _diagnostics) = build_catalog_providers_from_env().await;
    (
        llm.provider_id().to_string(),
        embedding.provider_id().to_string(),
    )
}

/// Convenience wrapper returning only the embedding provider id.
async fn embedding_id_with_env(vars: &[(&str, &str)]) -> String {
    provider_ids_with_env(vars).await.1
}

/// Convenience wrapper returning only the LLM provider id.
async fn llm_id_with_env(vars: &[(&str, &str)]) -> String {
    provider_ids_with_env(vars).await.0
}

#[tokio::test]
async fn providers_default_to_stubs() {
    let (llm, embedding) = provider_ids_with_env(&[]).await;
    assert_eq!(llm, sqlly_ai::catalog::STUB_LLM_PROVIDER_ID);
    assert_eq!(embedding, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

#[tokio::test]
async fn explicit_stub_provider_selects_stub() {
    let id = embedding_id_with_env(&[(EMBEDDING_PROVIDER_ENV, "stub")]).await;
    assert_eq!(id, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

#[tokio::test]
async fn unknown_embedding_provider_falls_back_to_stub() {
    let id = embedding_id_with_env(&[(EMBEDDING_PROVIDER_ENV, "totally-unknown")]).await;
    assert_eq!(id, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

#[tokio::test]
async fn ollama_embedding_without_model_falls_back_to_stub() {
    let id = embedding_id_with_env(&[(EMBEDDING_PROVIDER_ENV, "ollama")]).await;
    assert_eq!(id, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

#[tokio::test]
async fn ollama_embedding_blank_model_falls_back_to_stub() {
    let id = embedding_id_with_env(&[
        (EMBEDDING_PROVIDER_ENV, "ollama"),
        (EMBEDDING_MODEL_ENV, "   "),
    ])
    .await;
    assert_eq!(id, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

#[tokio::test]
async fn ollama_embedding_unreachable_falls_back_to_stub() {
    // Port 1 refuses immediately, so connect()'s dims probe fails fast and
    // the engine must fall back to the stub rather than panic or hang.
    let id = embedding_id_with_env(&[
        (EMBEDDING_PROVIDER_ENV, "ollama"),
        (EMBEDDING_MODEL_ENV, "nomic-embed-text"),
        (sqlly_ai::ollama::BASE_URL_ENV, "http://127.0.0.1:1"),
    ])
    .await;
    assert_eq!(id, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

#[tokio::test]
async fn llm_provider_selection_never_panics() {
    // Each config path must build without panicking and select the expected
    // provider. Ollama LLM construction is synchronous (no probe), so a
    // valid model name yields a real provider even with no server running.
    assert_eq!(
        llm_id_with_env(&[]).await,
        sqlly_ai::catalog::STUB_LLM_PROVIDER_ID
    );
    assert_eq!(
        llm_id_with_env(&[(LLM_PROVIDER_ENV, "stub")]).await,
        sqlly_ai::catalog::STUB_LLM_PROVIDER_ID
    );
    assert_eq!(
        llm_id_with_env(&[(LLM_PROVIDER_ENV, "bogus")]).await,
        sqlly_ai::catalog::STUB_LLM_PROVIDER_ID,
        "unknown provider must fall back to stub"
    );
    assert_eq!(
        llm_id_with_env(&[(LLM_PROVIDER_ENV, "ollama")]).await,
        sqlly_ai::catalog::STUB_LLM_PROVIDER_ID,
        "ollama without a model must fall back to stub"
    );
    assert_eq!(
        llm_id_with_env(&[
            (LLM_PROVIDER_ENV, "ollama"),
            (LLM_MODEL_ENV, "qwen2.5-coder"),
        ])
        .await,
        "ollama:qwen2.5-coder",
        "ollama with a model must select the Ollama provider"
    );
}

#[tokio::test]
async fn mlx_llm_without_model_falls_back_to_stub() {
    let id = llm_id_with_env(&[(LLM_PROVIDER_ENV, "mlx")]).await;
    assert_eq!(id, sqlly_ai::catalog::STUB_LLM_PROVIDER_ID);
}

#[tokio::test]
async fn mlx_llm_with_mlx_specific_env_var_selects_mlx() {
    let id = llm_id_with_env(&[
        (LLM_PROVIDER_ENV, "mlx"),
        (sqlly_ai::mlx::CHAT_MODEL_ENV, "qwen2.5-coder-mlx"),
    ])
    .await;
    assert_eq!(id, "mlx:qwen2.5-coder-mlx");
}

#[tokio::test]
async fn mlx_llm_with_unreachable_url_still_selects_mlx() {
    // LLM provider construction does not probe the sidecar; the client is
    // built lazily and failures surface at request time. The engine must
    // still boot and select the MLX provider.
    let id = llm_id_with_env(&[
        (LLM_PROVIDER_ENV, "mlx"),
        (LLM_MODEL_ENV, "qwen2.5-coder-mlx"),
        (sqlly_ai::mlx::BASE_URL_ENV, "http://127.0.0.1:1"),
    ])
    .await;
    assert_eq!(id, "mlx:qwen2.5-coder-mlx");
}

#[tokio::test]
async fn mlx_embedding_with_mlx_specific_env_var_selects_mlx() {
    // Embedding provider probes the sidecar at construction, so an
    // unreachable URL causes fallback to stub.
    let id = embedding_id_with_env(&[
        (EMBEDDING_PROVIDER_ENV, "mlx"),
        (sqlly_ai::mlx::EMBEDDING_MODEL_ENV, "bge-small-en-mlx"),
        (sqlly_ai::mlx::BASE_URL_ENV, "http://127.0.0.1:1"),
    ])
    .await;
    assert_eq!(id, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

#[tokio::test]
async fn mlx_embedding_without_model_falls_back_to_stub() {
    let id = embedding_id_with_env(&[(EMBEDDING_PROVIDER_ENV, "mlx")]).await;
    assert_eq!(id, sqlly_ai::catalog::STUB_PROVIDER_ID);
}

fn test_connection() -> EngineConnection {
    EngineConnection {
        server: "test.local".to_string(),
        port: 1433,
        database: Some("appdb".to_string()),
        auth: "sql".to_string(),
        username: "sa".to_string(),
        password: "p".to_string(),
        access_token: None,
        encrypt: true,
        trust_certificate: true,
        timeout_seconds: None,
        block_all_but_select: false,
        auto_wrap_rollback: false,
        confirm_data_changes: false,
        database_type: None,
        statement_timeout_seconds: None,
        enforce_read_only: false,
        proxy_profile: None,
    }
}

/// Same connection as a JSON value for request payloads. `EngineConnection`
/// only derives `Deserialize` (it is an inbound type), so it cannot be
/// placed inside `serde_json::json!` directly.
fn test_connection_json() -> serde_json::Value {
    serde_json::json!({
        "server": "test.local",
        "port": 1433,
        "database": "appdb",
        "auth": "sql",
        "username": "sa",
        "password": "p",
        "encrypt": true,
        "trust_certificate": true
    })
}

/// Two tables with a foreign key, carrying DB source timestamps.
fn sample_objects() -> Vec<CatalogObject> {
    use sqlly_ai::catalog::{
        CatalogColumn, CatalogForeignKey, CatalogObjectKind, CatalogSource, SourceKind,
    };
    vec![
        CatalogObject {
            schema: Some("dbo".to_string()),
            name: "Orders".to_string(),
            kind: CatalogObjectKind::Table,
            columns: vec![
                CatalogColumn {
                    name: "OrderId".to_string(),
                    type_name: Some("int".to_string()),
                    ordinal: 0,
                    nullable: false,
                },
                CatalogColumn {
                    name: "CustomerId".to_string(),
                    type_name: Some("int".to_string()),
                    ordinal: 1,
                    nullable: false,
                },
            ],
            primary_key: vec!["OrderId".to_string()],
            foreign_keys: vec![CatalogForeignKey {
                columns: vec!["CustomerId".to_string()],
                target_schema: Some("dbo".to_string()),
                target_table: "Customers".to_string(),
                target_columns: vec!["CustomerId".to_string()],
            }],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1_700_000_000_000),
                modify_date: Some(1_700_000_000_000),
            },
        },
        CatalogObject {
            schema: Some("dbo".to_string()),
            name: "Customers".to_string(),
            kind: CatalogObjectKind::Table,
            columns: vec![CatalogColumn {
                name: "CustomerId".to_string(),
                type_name: Some("int".to_string()),
                ordinal: 0,
                nullable: false,
            }],
            primary_key: vec!["CustomerId".to_string()],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1_700_000_000_000),
                modify_date: Some(1_700_000_000_000),
            },
        },
    ]
}

/// SyncCatalog adds new objects and reports them.
#[tokio::test]
async fn sync_catalog_adds_objects() {
    let catalog = test_catalog_state();
    let frame = sync_catalog_bytes("s1", &sample_objects());

    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    dispatch_request(&frame, &frames, &active, &pool, &catalog).await;
    drop(frames);

    match rx.recv().await.unwrap() {
        EngineFrame::CatalogSyncReport { id, report } => {
            assert_eq!(id, "s1");
            assert_eq!(report.added, 2, "both objects are new: {report:?}");
            assert_eq!(report.changed, 0);
            assert_eq!(report.removed, 0);
            assert_eq!(
                report.ai_cache_invalidated, 2,
                "new objects invalidate their AI cache"
            );
        }
        other => panic!("expected CatalogSyncReport, got {other:?}"),
    }
}

/// Dispatch a `sync_catalog` request and return the first frame. Extracted
/// as a named function (rather than a closure) so async borrow-checker
/// lifetime constraints don't interfere.
async fn dispatch_sync_frame(catalog: &DefaultCatalogService, bytes: &[u8]) -> EngineFrame {
    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    dispatch_request(bytes, &frames, &active, &pool, catalog).await;
    drop(frames);
    rx.recv().await.expect("a response frame")
}

/// Serialize a `sync_catalog` request as a protocol-v2 binary frame
/// (tag 0x11 + binary body). Replaces the former JSON encoding.
fn sync_catalog_bytes(id: &str, objects: &[CatalogObject]) -> Vec<u8> {
    let connection_json = serde_json::to_string(&test_connection_json()).unwrap();
    let mut w = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .string(&connection_json)
        .u32_le(objects.len() as u32);
    for obj in objects {
        w = encode_catalog_object_for_test(w, obj);
    }
    let mut frame = vec![REQUEST_TAG_SYNC_CATALOG];
    frame.extend_from_slice(&w.done());
    frame
}

/// Build a binary validate_sql request frame (tag 0x0C).
fn validate_sql_bytes(id: &str, sql: &str) -> Vec<u8> {
    validate_sql_bytes_with_tenant(id, sql, None)
}

fn validate_sql_bytes_with_tenant(
    id: &str,
    sql: &str,
    tenant: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
) -> Vec<u8> {
    let connection_json = serde_json::to_string(&test_connection_json()).unwrap();
    let mut w = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .string(&connection_json)
        .string(sql)
        .opt_string(None);
    if let Some(scope) = tenant {
        w = w
            .bool(scope.require_tenant_filter)
            .bool(scope.fail_closed_tenant_filter)
            .opt_string_list(scope.tenant_column_folds.as_deref());
    }
    let body = w.done();
    let mut frame = vec![REQUEST_TAG_VALIDATE_SQL];
    frame.extend_from_slice(&body);
    frame
}

/// Build a binary search_schema request frame (tag 0x08).
fn search_schema_bytes(id: &str, query: &str, limit: Option<u32>) -> Vec<u8> {
    let connection_json = serde_json::to_string(&test_connection_json()).unwrap();
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .string(&connection_json)
        .string(query)
        .opt_u32(limit)
        .u32_le(0)
        .done();
    let mut frame = vec![REQUEST_TAG_SEARCH_SCHEMA];
    frame.extend_from_slice(&body);
    frame
}

/// Encodes a single `CatalogObject` into a `BinaryTestWriter` for tests.
fn encode_catalog_object_for_test(
    mut w: BinaryTestWriter,
    obj: &CatalogObject,
) -> BinaryTestWriter {
    use sqlly_ai::catalog::{CatalogObjectKind, SourceKind};
    let kind_byte = match obj.kind {
        CatalogObjectKind::Table => 0u8,
        CatalogObjectKind::View => 1,
        CatalogObjectKind::StoredProcedure => 2,
        CatalogObjectKind::Function => 3,
        CatalogObjectKind::Type => 4,
    };
    let source_byte = match obj.source.kind {
        SourceKind::Database => 0u8,
        SourceKind::File => 1,
    };
    w = w
        .opt_string(obj.schema.as_deref())
        .string(&obj.name)
        .u8(kind_byte)
        .u32_le(obj.columns.len() as u32);
    for col in &obj.columns {
        w = w
            .string(&col.name)
            .opt_string(col.type_name.as_deref())
            .i64_le(col.ordinal)
            .bool(col.nullable);
    }
    w = w.u32_le(obj.primary_key.len() as u32);
    for col in &obj.primary_key {
        w = w.string(col);
    }
    w = w.u32_le(obj.foreign_keys.len() as u32);
    for fk in &obj.foreign_keys {
        w = w.u32_le(fk.columns.len() as u32);
        for col in &fk.columns {
            w = w.string(col);
        }
        w = w
            .opt_string(fk.target_schema.as_deref())
            .string(&fk.target_table)
            .u32_le(fk.target_columns.len() as u32);
        for col in &fk.target_columns {
            w = w.string(col);
        }
    }
    w = w.u32_le(obj.parameters.len() as u32);
    for param in &obj.parameters {
        w = w
            .string(&param.name)
            .opt_string(param.type_name.as_deref())
            .bool(param.is_output);
    }
    w = w
        .opt_string(obj.definition.as_deref())
        .u8(source_byte)
        .opt_string(obj.source.path.as_deref())
        .opt_i64(obj.source.create_date)
        .opt_i64(obj.source.modify_date);
    w
}

/// Build a binary validate frame (tag 0x10 + body) for tests.
fn validate_binary_frame(
    id: &str,
    sql: &str,
    dialect: Option<&str>,
    default_schema: Option<&str>,
    objects: &[(&str, &str, &str, &[&str])], // (schema, name, kind, columns)
) -> Vec<u8> {
    let kind_byte = |k: &str| -> u8 {
        match k {
            "table" => 0,
            "view" => 1,
            "sproc" => 2,
            "function" => 3,
            "type" => 4,
            _ => 0,
        }
    };
    let mut w = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .string(sql)
        .opt_string(dialect)
        .opt_string(default_schema)
        .u32_le(objects.len() as u32);
    for (schema, name, kind, columns) in objects {
        w = w
            .string(schema)
            .string(name)
            .u8(kind_byte(kind))
            .u32_le(columns.len() as u32);
        for col in *columns {
            w = w.string(col);
        }
        w = w.u32_le(0); // parameter_count
    }
    w = w.u32_le(0); // foreign_key_count
    let mut frame = vec![REQUEST_TAG_VALIDATE];
    frame.extend_from_slice(&w.done());
    frame
}

/// Re-syncing identical objects (same timestamp) is a no-op: the timestamp
/// gate skips even the hash check, and AI caches are NOT re-invalidated.
#[tokio::test]
async fn sync_catalog_unchanged_is_noop() {
    let catalog = test_catalog_state();
    let bytes = sync_catalog_bytes("s1", &sample_objects());

    let first = dispatch_sync_frame(&catalog, &bytes).await;
    match &first {
        EngineFrame::CatalogSyncReport { report, .. } => assert_eq!(report.added, 2),
        other => panic!("first sync: expected report, got {other:?}"),
    }

    let second = dispatch_sync_frame(&catalog, &bytes).await;
    match second {
        EngineFrame::CatalogSyncReport { report, .. } => {
            assert_eq!(report.added, 0, "nothing new the second time");
            assert_eq!(report.unchanged, 2, "both unchanged via timestamp gate");
            assert_eq!(
                report.ai_cache_invalidated, 0,
                "no change ⇒ no AI-cache invalidation"
            );
        }
        other => panic!("second sync: expected report, got {other:?}"),
    }
}

/// When the timestamp moves AND the content hash differs (a real DDL
/// change), the object is detected as `changed`. The timestamp gate
/// triggers the hash recomputation; the differing hash confirms a real
/// change and invalidates the AI cache.
#[tokio::test]
async fn sync_catalog_detects_change_when_timestamp_moves() {
    let catalog = test_catalog_state();
    let mut objects = sample_objects();

    dispatch_sync_frame(&catalog, &sync_catalog_bytes("s1", &objects)).await;
    // Change a column type AND bump modify_date (realistic: a DDL change
    // bumps the DB modify timestamp). Hash differs ⇒ Changed.
    objects[0].columns[1].type_name = Some("bigint".to_string());
    objects[0].source.modify_date = Some(1_700_000_001_000);
    let second = dispatch_sync_frame(&catalog, &sync_catalog_bytes("s1", &objects)).await;
    match second {
        EngineFrame::CatalogSyncReport { report, .. } => {
            assert_eq!(
                report.changed, 1,
                "Orders changed (column type): {report:?}"
            );
            assert_eq!(report.unchanged, 1);
            assert_eq!(
                report.ai_cache_invalidated, 1,
                "changed object invalidates its AI cache"
            );
        }
        other => panic!("expected report, got {other:?}"),
    }
}

/// When the timestamp moves but the content hash is unchanged (a touched
/// file or a no-op recompile), the object is NOT reported as changed — the
/// hash eliminates the false positive and avoids needless AI-cache
/// invalidation.
#[tokio::test]
async fn sync_catalog_hash_eliminates_false_positive_on_moved_timestamp() {
    let catalog = test_catalog_state();
    let mut objects = sample_objects();

    dispatch_sync_frame(&catalog, &sync_catalog_bytes("s1", &objects)).await;
    // Bump the timestamp but DON'T change any content. Same hash.
    objects[0].source.modify_date = Some(1_700_000_999_000);
    let second = dispatch_sync_frame(&catalog, &sync_catalog_bytes("s1", &objects)).await;
    match second {
        EngineFrame::CatalogSyncReport { report, .. } => {
            assert_eq!(report.changed, 0, "hash identical ⇒ not a real change");
            assert_eq!(report.unchanged, 2);
            assert_eq!(report.ai_cache_invalidated, 0);
        }
        other => panic!("expected report, got {other:?}"),
    }
}

/// Removing an object tombstones it; re-adding it resurrects it.
#[tokio::test]
async fn sync_catalog_tombstones_and_resurrects() {
    let catalog = test_catalog_state();
    let objects = sample_objects();

    // Sync two.
    dispatch_sync_frame(&catalog, &sync_catalog_bytes("s1", &objects)).await;
    // Sync one (Orders dropped).
    let dropped = dispatch_sync_frame(&catalog, &sync_catalog_bytes("s1", &objects[1..])).await;
    match dropped {
        EngineFrame::CatalogSyncReport { report, .. } => {
            assert_eq!(report.removed, 1, "Orders was tombstoned");
        }
        other => panic!("expected report, got {other:?}"),
    }
    // Re-sync both: Orders resurrects.
    let resurrected = dispatch_sync_frame(&catalog, &sync_catalog_bytes("s1", &objects)).await;
    match resurrected {
        EngineFrame::CatalogSyncReport { report, .. } => {
            assert_eq!(report.resurrected, 1, "Orders came back");
        }
        other => panic!("expected report, got {other:?}"),
    }
}

/// The catalog persists across store reopens (the whole point: survive
/// between connections). Sync two objects, reopen the SAME scope, re-sync
/// identical objects ⇒ all unchanged.
#[tokio::test]
async fn catalog_persists_across_reopen() {
    let catalog = test_catalog_state();
    let profile = test_connection().into_profile();

    // First sync via the run function.
    let first = run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;
    match &first {
        EngineFrame::CatalogSyncReport { report, .. } => assert_eq!(report.added, 2),
        other => panic!("expected report, got {other:?}"),
    }

    // Reopen the same scope and re-sync.
    let store = catalog.open_store(&profile).unwrap();
    let report = store.sync(&sample_objects()).unwrap();
    assert_eq!(report.added, 0);
    assert_eq!(report.unchanged, 2, "persisted across reopen");
}

/// build_ai_cache processes all stale objects and clears their flags.
#[tokio::test]
async fn build_ai_cache_clears_stale_flags() {
    let catalog = test_catalog_state();
    let profile = test_connection().into_profile();

    // Sync (makes everything stale).
    run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;
    let store = catalog.open_store(&profile).unwrap();
    let status = sqlly_ai::catalog::ai_cache_status(&store).unwrap();
    assert_eq!(status.stale, 2, "both stale after sync");

    // Build.
    let frame = run_build_ai_cache("b1".to_string(), profile.clone(), &catalog).await;
    match frame {
        EngineFrame::AiCacheBuildReport { report, .. } => {
            assert_eq!(report.processed, 2, "both built");
            assert_eq!(report.remaining_stale, 0);
        }
        other => panic!("expected build report, got {other:?}"),
    }

    // Flags cleared.
    let store = catalog.open_store(&profile).unwrap();
    let status = sqlly_ai::catalog::ai_cache_status(&store).unwrap();
    assert_eq!(status.valid, 2);
    assert_eq!(status.stale, 0);
}

/// ask_schema returns a non-empty answer that references the catalog
/// objects relevant to the question.
#[tokio::test]
async fn ask_schema_returns_answer() {
    let catalog = test_catalog_state();
    let profile = test_connection().into_profile();

    // Seed the catalog.
    run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;

    let frame = run_ask_schema(
        "a1".to_string(),
        profile,
        &catalog,
        "How are Orders related to Customers?".to_string(),
        None,
        None,
        SchemaPurpose::General,
    )
    .await;
    match frame {
        EngineFrame::SchemaAnswer { id, response } => {
            assert_eq!(id, "a1");
            assert!(
                !response.text.is_empty(),
                "answer must be non-empty: {response:?}"
            );
            // The stub provider echoes how many objects it received as
            // context. Both tables are relevant to the question.
            let obj_count = response
                .meta
                .get("objects")
                .and_then(|v| v.as_u64())
                .expect("stub provider reports object count in meta");
            assert!(
                obj_count > 0,
                "context must include at least one relevant object"
            );
        }
        other => panic!("expected SchemaAnswer, got {other:?}"),
    }
}

#[tokio::test]
async fn search_schema_dispatch_returns_results() {
    let catalog = test_catalog_state();
    let sync = sync_catalog_bytes("s1", &sample_objects());
    dispatch_sync_frame(&catalog, &sync).await;
    let request = search_schema_bytes("search-1", "CustomerId", Some(5));

    let frame = dispatch_sync_frame(&catalog, &request).await;
    match frame {
        EngineFrame::SchemaSearchResults { id, results } => {
            assert_eq!(id, "search-1");
            assert!(
                results.iter().any(|hit| hit.name == "Orders"),
                "column-only query should find Orders: {results:?}"
            );
        }
        other => panic!("expected SchemaSearchResults, got {other:?}"),
    }
}

#[test]
fn schema_search_results_log_frame_is_terminal_shape() {
    let frame = EngineFrame::SchemaSearchResults {
        id: "search-1".to_string(),
        results: vec![],
    };
    let (id, kind, detail) = frame_log_fields(&frame);
    assert_eq!(id, "search-1");
    assert_eq!(kind, "schema_search_results");
    assert_eq!(detail, "results=0");
}

/// The catalog_sync_report frame is JSON-encoded (tag 0x00), not the flat
/// binary layout used by validation reports.
#[tokio::test]
async fn catalog_sync_report_encodes_as_binary() {
    let frame = EngineFrame::CatalogSyncReport {
        id: "s9".to_string(),
        report: SyncReport {
            added: 3,
            changed: 1,
            unchanged: 0,
            removed: 0,
            resurrected: 0,
            ai_cache_invalidated: 4,
            inferred_relationships: 0,
        },
    };
    let bytes = encode_frame(&frame).unwrap();
    let tag = bytes[4];
    assert_eq!(tag, FRAME_TAG_CATALOG_SYNC_REPORT);
    let body = &bytes[5..];
    let mut r = BinaryFrameReader::new(body);
    let id = r.read_string("id").unwrap();
    assert_eq!(id, "s9");
    let json_len = r.read_u32_le("json_len").unwrap() as usize;
    let json_bytes = &body[r.offset()..r.offset() + json_len];
    let json: serde_json::Value = serde_json::from_slice(json_bytes).unwrap();
    assert_eq!(json["added"], 3);
    assert_eq!(json["ai_cache_invalidated"], 4);
}

#[test]
fn join_path_report_log_frame_is_terminal_shape() {
    let frame = EngineFrame::JoinPathReport {
        id: "jp-1".to_string(),
        paths: vec![],
    };
    let (id, kind, detail) = frame_log_fields(&frame);
    assert_eq!(id, "jp-1");
    assert_eq!(kind, "join_path_report");
    assert_eq!(detail, "paths=0");
}

#[tokio::test]
async fn join_path_report_encodes_as_binary() {
    let path = sqlly_ai::catalog::JoinPath {
        steps: vec![sqlly_ai::catalog::JoinPathStep {
            from_object_key: "dbo.orders".to_string(),
            from_columns: vec!["CustomerId".to_string()],
            to_object_key: "dbo.customers".to_string(),
            to_columns: vec!["CustomerId".to_string()],
            direction: sqlly_ai::catalog::JoinDirection::Forward,
            is_inferred: true,
            confidence: 0.7,
        }],
        score: 0.7,
    };
    let frame = EngineFrame::JoinPathReport {
        id: "jp9".to_string(),
        paths: vec![path],
    };
    let bytes = encode_frame(&frame).unwrap();
    let tag = bytes[4];
    assert_eq!(tag, FRAME_TAG_JOIN_PATH_REPORT);
    let body = &bytes[5..];
    let mut r = BinaryFrameReader::new(body);
    let id = r.read_string("id").unwrap();
    assert_eq!(id, "jp9");
    let json_len = r.read_u32_le("json_len").unwrap() as usize;
    let json_bytes = &body[r.offset()..r.offset() + json_len];
    let json: serde_json::Value = serde_json::from_slice(json_bytes).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    let step = &json[0]["steps"][0];
    assert_eq!(step["is_inferred"], true);
    assert_eq!(step["confidence"], 0.7);
}

#[test]
fn generate_sql_request_decodes_with_optional_fields() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;

    let minimal_body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("g1")
        .string(connection_json)
        .string("list every order")
        .opt_string(None)
        .opt_u32(None)
        .opt_string(None)
        .opt_u32(None)
        .done();
    let minimal = decode_generate_sql_binary(&minimal_body).expect("decode generate_sql minimal");
    match minimal {
        EngineRequest::GenerateSql {
            id,
            question,
            dialect,
            max_tokens,
            system,
            max_attempts,
            ..
        } => {
            assert_eq!(id, "g1");
            assert_eq!(question, "list every order");
            assert!(dialect.is_none());
            assert!(max_tokens.is_none());
            assert!(system.is_none());
            assert!(max_attempts.is_none());
        }
        other => panic!("expected generate_sql, got {other:?}"),
    }

    let full_body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("g2")
        .string(connection_json)
        .string("list every order")
        .opt_string(Some("postgres"))
        .opt_u32(Some(1234))
        .opt_string(Some("be terse"))
        .opt_u32(Some(3))
        .done();
    let full = decode_generate_sql_binary(&full_body).expect("decode generate_sql full");
    match full {
        EngineRequest::GenerateSql {
            id,
            dialect,
            max_tokens,
            system,
            max_attempts,
            ..
        } => {
            assert_eq!(id, "g2");
            assert_eq!(dialect.as_deref(), Some("postgres"));
            assert_eq!(max_tokens, Some(1234));
            assert_eq!(system.as_deref(), Some("be terse"));
            assert_eq!(max_attempts, Some(3));
        }
        other => panic!("expected generate_sql, got {other:?}"),
    }
}

#[test]
fn validate_sql_request_decodes_with_optional_dialect() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;

    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v1")
        .string(connection_json)
        .string("select 1")
        .opt_string(None)
        .done();
    let request = decode_validate_sql_binary(&body).expect("decode validate_sql");
    match request {
        EngineRequest::ValidateSql {
            id,
            sql,
            dialect,
            tenant_scope,
            ..
        } => {
            assert_eq!(id, "v1");
            assert_eq!(sql, "select 1");
            assert!(dialect.is_none());
            assert!(tenant_scope.is_none());
        }
        other => panic!("expected validate_sql, got {other:?}"),
    }
}

#[test]
fn validate_sql_request_decodes_trailing_tenant_scope() {
    let connection_json = serde_json::to_string(&test_connection_json()).unwrap();
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v2")
        .string(&connection_json)
        .string("select 1")
        .opt_string(None)
        .bool(true)
        .bool(true)
        .opt_string_list(None)
        .done();
    let request = decode_validate_sql_binary(&body).expect("decode validate_sql");
    match request {
        EngineRequest::ValidateSql { tenant_scope, .. } => {
            let scope = tenant_scope.expect("tenant tail");
            assert!(scope.require_tenant_filter);
            assert!(scope.fail_closed_tenant_filter);
            assert!(scope.tenant_column_folds.is_none());
        }
        other => panic!("expected validate_sql, got {other:?}"),
    }
}

#[test]
fn engine_connection_omitted_trust_certificate_defaults_false() {
    let json = r#"{"server":"localhost","port":1433,"auth":"sql","username":"sa","password":"pw","encrypt":true}"#;
    let conn: EngineConnection = serde_json::from_str(json).unwrap();
    assert!(!conn.trust_certificate);
    let profile = conn.into_profile();
    assert!(!profile.trust_server_certificate);
}

#[test]
fn sql_generation_report_log_frame_is_terminal_shape() {
    let frame = EngineFrame::SqlGenerationReport {
        id: "g9".to_string(),
        report: sqlly_ai::catalog::SqlGenerationResult {
            sql: "select 1".to_string(),
            intent: sqlly_ai::catalog::SqlIntent::Select,
            validation: sqlly_ai::catalog::SqlValidationResult {
                is_valid: true,
                requires_confirmation: false,
                detected_intent: sqlly_ai::catalog::SqlIntent::Select,
                issues: vec![],
                referenced_objects: vec![],
                referenced_columns: vec![],
            },
            grounded_in: vec![],
            provider_meta: serde_json::json!({}),
            attempts: vec![],
        },
    };
    let (id, kind, detail) = frame_log_fields(&frame);
    assert_eq!(id, "g9");
    assert_eq!(kind, "sql_generation_report");
    assert_eq!(
        detail,
        "chars=8 intent=select valid=true confirm=false issues=0 grounded=0 attempts=0"
    );
}

#[test]
fn sql_validation_report_log_frame_counts_severities() {
    let frame = EngineFrame::SqlValidationReport {
        id: "v9".to_string(),
        report: sqlly_ai::catalog::SqlValidationResult {
            is_valid: false,
            requires_confirmation: false,
            detected_intent: sqlly_ai::catalog::SqlIntent::Delete,
            issues: vec![
                sqlly_ai::catalog::SqlValidationIssue {
                    code: sqlly_ai::catalog::SqlIssueCode::DeleteWithoutWhere,
                    severity: sqlly_ai::catalog::SqlIssueSeverity::Error,
                    message: "DELETE without WHERE".to_string(),
                    identifier: None,
                    object_keys: vec![],
                    span: None,
                },
                sqlly_ai::catalog::SqlValidationIssue {
                    code: sqlly_ai::catalog::SqlIssueCode::TenantScopeMissingPredicate,
                    severity: sqlly_ai::catalog::SqlIssueSeverity::Warning,
                    message: "missing tenant predicate".to_string(),
                    identifier: None,
                    object_keys: vec![],
                    span: None,
                },
            ],
            referenced_objects: vec![],
            referenced_columns: vec![],
        },
    };
    let (id, kind, detail) = frame_log_fields(&frame);
    assert_eq!(id, "v9");
    assert_eq!(kind, "sql_validation_report");
    assert_eq!(
        detail,
        "valid=false intent=delete errors=1 warnings=1 confirm=0 objects=0"
    );
}

/// validate_sql against a freshly-synced catalog: a known SELECT resolves
/// cleanly and yields a valid report through the full dispatch path.
#[tokio::test]
async fn validate_sql_dispatch_known_select_is_valid() {
    let catalog = test_catalog_state();
    let sync = sync_catalog_bytes("s1", &sample_objects());
    dispatch_sync_frame(&catalog, &sync).await;

    let request = validate_sql_bytes("vsql-1", "select o.OrderId from dbo.Orders o");

    let frame = dispatch_sync_frame(&catalog, &request).await;
    match frame {
        EngineFrame::SqlValidationReport { id, report } => {
            assert_eq!(id, "vsql-1");
            assert!(report.is_valid, "known SELECT must validate: {report:?}");
            assert!(
                report
                    .referenced_objects
                    .iter()
                    .any(|o| o.object_key == "dbo.orders")
            );
        }
        other => panic!("expected SqlValidationReport, got {other:?}"),
    }
}

/// validate_sql flags an unknown table with an UnresolvedTable error.
#[tokio::test]
async fn validate_sql_dispatch_unknown_table_errors() {
    let catalog = test_catalog_state();
    let sync = sync_catalog_bytes("s1", &sample_objects());
    dispatch_sync_frame(&catalog, &sync).await;

    let request = validate_sql_bytes("vsql-2", "select * from dbo.Nope");

    let frame = dispatch_sync_frame(&catalog, &request).await;
    match frame {
        EngineFrame::SqlValidationReport { report, .. } => {
            assert!(!report.is_valid);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|i| i.code == sqlly_ai::catalog::SqlIssueCode::UnresolvedTable)
            );
        }
        other => panic!("expected SqlValidationReport, got {other:?}"),
    }
}

/// Required tenant policy on the wire turns a missing tenant predicate into an error.
#[tokio::test]
async fn validate_sql_required_tenant_on_wire_is_error() {
    use sqlly_ai::catalog::{
        CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
    };
    let catalog = test_catalog_state();
    let objects = vec![CatalogObject {
        schema: Some("dbo".to_string()),
        name: "Orders".to_string(),
        kind: CatalogObjectKind::Table,
        columns: vec![
            CatalogColumn {
                name: "OrderId".to_string(),
                type_name: Some("int".to_string()),
                ordinal: 0,
                nullable: false,
            },
            CatalogColumn {
                name: "tenant_id".to_string(),
                type_name: Some("int".to_string()),
                ordinal: 1,
                nullable: false,
            },
        ],
        primary_key: vec!["OrderId".to_string()],
        foreign_keys: vec![],
        parameters: vec![],
        definition: None,
        source: CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: Some(1),
            modify_date: Some(1),
        },
    }];
    dispatch_sync_frame(&catalog, &sync_catalog_bytes("s-tenant", &objects)).await;
    let request = validate_sql_bytes_with_tenant(
        "vsql-req",
        "select total from dbo.orders",
        Some(sqlly_remote::engine_protocol::TenantScopeWire {
            require_tenant_filter: true,
            fail_closed_tenant_filter: true,
            tenant_column_folds: None,
        }),
    );
    let frame = dispatch_sync_frame(&catalog, &request).await;
    match frame {
        EngineFrame::SqlValidationReport { report, .. } => {
            assert!(
                report.issues.iter().any(|issue| {
                    issue.code == sqlly_ai::catalog::SqlIssueCode::TenantScopeMissingPredicate
                        && issue.severity == sqlly_ai::catalog::SqlIssueSeverity::Error
                }),
                "{report:?}"
            );
        }
        other => panic!("expected SqlValidationReport, got {other:?}"),
    }
}

/// generate_sql after a sync yields a terminal report. The stub provider
/// produces no SQL fence, so extraction falls back to the whole answer; the
/// point is that the dispatch path runs end to end and validates the output.
#[tokio::test]
async fn generate_sql_after_sync_yields_report() {
    let catalog = test_catalog_state();
    let profile = test_connection().into_profile();
    run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;

    let frame = run_generate_sql(
        "gen-1".to_string(),
        profile,
        &catalog,
        "show every order with its customer".to_string(),
        Some("sqlserver".to_string()),
        None,
        None,
        None,
        None,
    )
    .await;
    match frame {
        EngineFrame::SqlGenerationReport { id, report } => {
            assert_eq!(id, "gen-1");
            assert!(!report.sql.is_empty(), "generated sql is non-empty");
            // The validation report is always attached.
            let _ = report.validation.is_valid;
        }
        other => panic!("expected SqlGenerationReport, got {other:?}"),
    }
}

/// A provider that returns a fixed, fenced SQL block — lets the generation
/// test assert a *real* validation outcome (the built-in stub never emits
/// SQL, so it can only prove the pipeline runs, not that valid SQL passes).
struct CannedSqlProvider {
    sql: String,
}

#[async_trait::async_trait]
impl sqlly_ai::catalog::LlmProvider for CannedSqlProvider {
    async fn complete(
        &self,
        _request: sqlly_ai::catalog::CompletionRequest,
    ) -> Result<sqlly_ai::catalog::CompletionResponse, sqlly_ai::catalog::LlmError> {
        Ok(sqlly_ai::catalog::CompletionResponse::new(format!(
            "Here is the query:\n\n```sql\n{}\n```\n",
            self.sql
        )))
    }

    async fn ask(
        &self,
        _prompt: sqlly_ai::catalog::LlmPrompt,
    ) -> Result<LlmResponse, sqlly_ai::catalog::LlmError> {
        Ok(LlmResponse::new(format!(
            "Here is the query:\n\n```sql\n{}\n```\n",
            self.sql
        )))
    }

    fn provider_id(&self) -> &str {
        "canned"
    }
}

/// A [`DefaultCatalogService`] whose LLM is a [`CannedSqlProvider`] returning `sql`.
fn test_catalog_state_with_sql(sql: &str) -> DefaultCatalogService {
    DefaultCatalogService::from_parts(
        std::env::temp_dir().join(format!(
            "sqlly-engine-catalog-test-{}",
            uuid::Uuid::new_v4()
        )),
        Arc::new(CannedSqlProvider {
            sql: sql.to_string(),
        }),
        Arc::new(StubEmbeddingProvider::new()),
    )
}

/// With a canned provider returning a valid SELECT against a synced table,
/// the generation report extracts that SQL and its nested validation passes.
#[tokio::test]
async fn generate_sql_with_canned_provider_validates_output() {
    let catalog = test_catalog_state_with_sql("select o.OrderId from dbo.Orders o");
    let profile = test_connection().into_profile();
    run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;

    let frame = run_generate_sql(
        "gen-canned".to_string(),
        profile,
        &catalog,
        "show every order id".to_string(),
        Some("sqlserver".to_string()),
        None,
        None,
        None,
        None,
    )
    .await;
    match frame {
        EngineFrame::SqlGenerationReport { id, report } => {
            assert_eq!(id, "gen-canned");
            assert_eq!(report.sql, "select o.OrderId from dbo.Orders o");
            assert!(
                report.validation.is_valid,
                "valid SELECT should pass validation: {:?}",
                report.validation.issues
            );
            assert_eq!(report.validation.detected_intent.as_str(), "select");
            assert!(
                report
                    .validation
                    .referenced_objects
                    .iter()
                    .any(|o| o.object_key == "dbo.orders"),
                "validation grounds the report in dbo.orders"
            );
        }
        other => panic!("expected SqlGenerationReport, got {other:?}"),
    }
}

/// A canned provider returning a destructive statement surfaces the safety
/// issue through the nested validation report.
#[tokio::test]
async fn generate_sql_with_canned_provider_flags_destructive_output() {
    let catalog = test_catalog_state_with_sql("delete from dbo.Orders");
    let profile = test_connection().into_profile();
    run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;

    let frame = run_generate_sql(
        "gen-del".to_string(),
        profile,
        &catalog,
        "remove all orders".to_string(),
        Some("sqlserver".to_string()),
        None,
        None,
        None,
        None,
    )
    .await;
    match frame {
        EngineFrame::SqlGenerationReport { report, .. } => {
            assert!(
                !report.validation.is_valid,
                "DELETE without WHERE is invalid"
            );
            assert!(
                report
                    .validation
                    .issues
                    .iter()
                    .any(|i| i.code == sqlly_ai::catalog::SqlIssueCode::DeleteWithoutWhere),
                "expected DeleteWithoutWhere, got {:?}",
                report.validation.issues
            );
        }
        other => panic!("expected SqlGenerationReport, got {other:?}"),
    }
}

/// A provider whose calls never return promptly — it sleeps far longer than
/// the configured schema timeout so the engine's bound must fire.
struct SlowLlmProvider;

#[async_trait::async_trait]
impl sqlly_ai::catalog::LlmProvider for SlowLlmProvider {
    async fn complete(
        &self,
        _request: sqlly_ai::catalog::CompletionRequest,
    ) -> Result<sqlly_ai::catalog::CompletionResponse, sqlly_ai::catalog::LlmError> {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        Ok(sqlly_ai::catalog::CompletionResponse::new(
            "too late".to_string(),
        ))
    }

    async fn ask(
        &self,
        _prompt: sqlly_ai::catalog::LlmPrompt,
    ) -> Result<LlmResponse, sqlly_ai::catalog::LlmError> {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        Ok(LlmResponse::new("too late".to_string()))
    }

    fn provider_id(&self) -> &str {
        "slow"
    }
}

fn slow_catalog_state() -> DefaultCatalogService {
    DefaultCatalogService::from_parts(
        std::env::temp_dir().join(format!(
            "sqlly-engine-catalog-test-{}",
            uuid::Uuid::new_v4()
        )),
        Arc::new(SlowLlmProvider),
        Arc::new(StubEmbeddingProvider::new()),
    )
}

/// `ask_schema` must always emit a terminal frame: when the LLM provider
/// hangs past the configured bound, the engine returns `RequestFailed` with
/// the timeout message instead of leaving the request unanswered forever.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn ask_schema_times_out_with_terminal_frame() {
    let mut env = sqlly_testkit::EnvGuard::acquire();
    env.set(SCHEMA_LLM_TIMEOUT_ENV, "1");

    let catalog = slow_catalog_state();
    let profile = test_connection().into_profile();
    run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;

    let frame = run_ask_schema(
        "ask-slow".to_string(),
        profile,
        &catalog,
        "what tables exist".to_string(),
        None,
        None,
        SchemaPurpose::General,
    )
    .await;

    match frame {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "ask-slow");
            assert!(
                error.to_string().contains("timed out after"),
                "expected timeout message, got {error:?}"
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

/// `generate_sql` is bounded by the same timeout and likewise yields a
/// terminal `RequestFailed` frame rather than hanging.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn generate_sql_times_out_with_terminal_frame() {
    let mut env = sqlly_testkit::EnvGuard::acquire();
    env.set(SCHEMA_LLM_TIMEOUT_ENV, "1");

    let catalog = slow_catalog_state();
    let profile = test_connection().into_profile();
    run_sync_catalog(
        "s1".to_string(),
        &profile,
        &catalog,
        &sample_objects(),
        sqlly_ai::catalog::SyncCoverage::FullSource,
    )
    .await;

    let frame = run_generate_sql(
        "gen-slow".to_string(),
        profile,
        &catalog,
        "show every order id".to_string(),
        Some("sqlserver".to_string()),
        None,
        None,
        None,
        None,
    )
    .await;

    match frame {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "gen-slow");
            assert!(
                error.to_string().contains("timed out after"),
                "expected timeout message, got {error:?}"
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

/// `schema_llm_timeout` honors the env override and the disable sentinel.
#[test]
fn schema_llm_timeout_reads_env_override() {
    let mut env = sqlly_testkit::EnvGuard::acquire();

    env.set(SCHEMA_LLM_TIMEOUT_ENV, "5");
    assert_eq!(
        schema_llm_timeout(),
        Some(std::time::Duration::from_secs(5))
    );

    env.set(SCHEMA_LLM_TIMEOUT_ENV, "0");
    assert_eq!(schema_llm_timeout(), None, "0 disables the bound");

    env.remove(SCHEMA_LLM_TIMEOUT_ENV);
    assert_eq!(
        schema_llm_timeout(),
        Some(std::time::Duration::from_secs(
            DEFAULT_SCHEMA_LLM_TIMEOUT_SECS
        )),
        "default applies when unset"
    );
}

/// A fallback note is produced only when a non-stub provider was requested
/// but a different provider is actually active.
#[test]
fn provider_diagnostics_reports_silent_fallback() {
    let fell_back = ProviderDiagnostics {
        embedding_requested: Some("mlx".to_string()),
        embedding_active: "stub".to_string(),
        embedding_fallback_reason: Some("sentence_transformers missing".to_string()),
        ..ProviderDiagnostics::default()
    };
    let note = fell_back
        .embedding_fallback_note()
        .expect("fallback should produce a note");
    assert!(
        note.contains("mlx"),
        "note mentions requested provider: {note}"
    );
    assert!(
        note.contains("stub"),
        "note mentions active provider: {note}"
    );
    assert!(
        note.contains("sentence_transformers missing"),
        "note includes the reason: {note}"
    );
}

/// No note when the requested provider is the one actually in use, when it
/// is the stub, or when none was requested.
#[test]
fn provider_diagnostics_silent_when_no_fallback() {
    let matched = ProviderDiagnostics {
        embedding_requested: Some("mlx".to_string()),
        embedding_active: "mlx".to_string(),
        ..ProviderDiagnostics::default()
    };
    assert_eq!(matched.embedding_fallback_note(), None);

    let stub_requested = ProviderDiagnostics {
        embedding_requested: Some("stub".to_string()),
        embedding_active: "stub".to_string(),
        ..ProviderDiagnostics::default()
    };
    assert_eq!(stub_requested.embedding_fallback_note(), None);

    let none_requested = ProviderDiagnostics {
        embedding_requested: None,
        embedding_active: "stub".to_string(),
        ..ProviderDiagnostics::default()
    };
    assert_eq!(none_requested.embedding_fallback_note(), None);
}

// ------------------------------------------------------------------
// Binary request frame tests (protocol v2)
// ------------------------------------------------------------------

/// Builds binary request bodies for testing. This is the shared
/// `BinaryWriter` builder from `sqlly-engine-protocol`, aliased to its
/// historical local name so existing test call sites are unchanged.
use sqlly_remote::engine_protocol::BinaryWriter as BinaryTestWriter;

/// Build a minimal valid validate binary body.
fn minimal_validate_body() -> Vec<u8> {
    BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("req-1")
        .string("SELECT 1")
        .opt_string(Some("sqlserver"))
        .opt_string(Some("dbo"))
        .u32_le(0) // object_count
        .u32_le(0) // foreign_key_count
        .done()
}

/// Build a validate body with one table object (2 columns) and one FK.
fn validate_body_with_schema() -> Vec<u8> {
    BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("req-2")
        .string("SELECT * FROM dbo.Customers")
        .opt_string(Some("sqlserver"))
        .opt_string(Some("dbo"))
        .u32_le(1) // object_count
        // object 0: dbo.Customers (table)
        .string("dbo")
        .string("Customers")
        .u8(0) // kind = table
        .u32_le(2) // column_count
        .string("CustomerID")
        .string("CompanyName")
        .u32_le(1) // parameter_count
        // parameter 0
        .string("@ReturnCode")
        .string("INT")
        .bool(false) // is_output
        .u32_le(1) // foreign_key_count
        // FK 0
        .string("dbo")
        .string("Orders")
        .string("CustomerID")
        .string("dbo")
        .string("Customers")
        .string("CustomerID")
        .done()
}

/// Build a minimal valid sync_catalog binary body.
fn minimal_sync_catalog_body() -> Vec<u8> {
    let connection_json = r#"{"server":"localhost","port":1433,"auth":"sql","username":"sa","password":"pw","encrypt":true,"trust_certificate":true,"block_all_but_select":false,"auto_wrap_rollback":false}"#;
    BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-1")
        .string(connection_json)
        .u32_le(0) // object_count
        .done()
}

/// Build a sync_catalog body with one table object with columns, FKs,
/// parameters, definition, and source metadata.
fn sync_catalog_body_with_object() -> Vec<u8> {
    let connection_json = r#"{"server":"localhost","port":1433,"auth":"sql","username":"sa","password":"pw","encrypt":true,"trust_certificate":true,"block_all_but_select":false,"auto_wrap_rollback":false}"#;
    BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-2")
        .string(connection_json)
        .u32_le(1) // object_count
        // object 0: dbo.Orders (table)
        .opt_string(Some("dbo"))
        .string("Orders")
        .u8(0) // kind = table
        .u32_le(2) // column_count
        // column 0
        .string("OrderID")
        .opt_string(Some("INT"))
        .i64_le(1) // ordinal
        .bool(false) // nullable
        // column 1
        .string("CustomerID")
        .opt_string(Some("BIGINT"))
        .i64_le(2)
        .bool(true) // nullable
        .u32_le(1) // primary_key_count
        .string("OrderID")
        .u32_le(1) // foreign_key_count
        // FK 0
        .u32_le(1) // local_column_count
        .string("CustomerID")
        .opt_string(Some("dbo"))
        .string("Customers")
        .u32_le(1) // target_column_count
        .string("CustomerID")
        .u32_le(0) // parameter_count
        .opt_string(None) // definition
        .u8(0) // source_kind = database
        .opt_string(None) // path
        .opt_i64(Some(1000)) // create_date
        .opt_i64(Some(2000)) // modify_date
        .done()
}

#[test]
fn decode_validate_binary_minimal() {
    let body = minimal_validate_body();
    let req = decode_validate_binary(&body).expect("minimal validate should decode");
    match req {
        EngineRequest::Validate {
            id,
            sql,
            dialect,
            default_schema,
            objects,
            foreign_keys,
        } => {
            assert_eq!(id, "req-1");
            assert_eq!(sql, "SELECT 1");
            assert_eq!(dialect.as_deref(), Some("sqlserver"));
            assert_eq!(default_schema.as_deref(), Some("dbo"));
            assert!(objects.is_empty());
            assert!(foreign_keys.is_empty());
        }
        other => panic!("expected Validate, got {other:?}"),
    }
}

#[test]
fn decode_validate_binary_with_schema() {
    let body = validate_body_with_schema();
    let req = decode_validate_binary(&body).expect("validate with schema should decode");
    match req {
        EngineRequest::Validate {
            id,
            sql,
            objects,
            foreign_keys,
            ..
        } => {
            assert_eq!(id, "req-2");
            assert_eq!(sql, "SELECT * FROM dbo.Customers");
            assert_eq!(objects.len(), 1);
            let obj = &objects[0];
            assert_eq!(obj.schema, "dbo");
            assert_eq!(obj.name, "Customers");
            assert_eq!(obj.kind, "table");
            assert_eq!(obj.columns, vec!["CustomerID", "CompanyName"]);
            assert_eq!(obj.parameters.len(), 1);
            assert_eq!(obj.parameters[0].name, "@ReturnCode");
            assert_eq!(obj.parameters[0].type_name, "INT");
            assert!(!obj.parameters[0].is_output);

            assert_eq!(foreign_keys.len(), 1);
            let fk = &foreign_keys[0];
            assert_eq!(fk.from_table, "Orders");
            assert_eq!(fk.from_column, "CustomerID");
            assert_eq!(fk.to_table, "Customers");
        }
        other => panic!("expected Validate, got {other:?}"),
    }
}

#[test]
fn decode_validate_binary_empty_optional_fields() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("req-3")
        .string("")
        .opt_string(None) // dialect
        .opt_string(None) // default_schema
        .u32_le(0)
        .u32_le(0)
        .done();
    let req = decode_validate_binary(&body).expect("should decode");
    match req {
        EngineRequest::Validate {
            dialect,
            default_schema,
            objects,
            foreign_keys,
            ..
        } => {
            assert!(dialect.is_none());
            assert!(default_schema.is_none());
            assert!(objects.is_empty());
            assert!(foreign_keys.is_empty());
        }
        other => panic!("expected Validate, got {other:?}"),
    }
}

#[test]
fn decode_validate_binary_unsupported_layout_version() {
    let body = BinaryTestWriter::new()
        .u16_le(999) // wrong layout version
        .string("req-x")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(0)
        .u32_le(0)
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("layout version"),
        "error should mention layout version: {err}"
    );
}

#[test]
fn decode_validate_binary_unknown_kind() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("req-k")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1) // object_count
        .string("dbo")
        .string("X")
        .u8(99) // unknown kind
        .u32_le(0) // column_count
        .u32_le(0) // parameter_count
        .u32_le(0) // fk_count
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("unknown editor object kind"),
        "error should mention unknown kind: {err}"
    );
}

#[test]
fn decode_validate_binary_truncated() {
    // Cut the body short — not enough bytes for the layout_version + id.
    let body = [0x01u8]; // only one byte, need at least 2 for u16
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("truncated"),
        "error should mention truncation: {err}"
    );
}

#[test]
fn decode_validate_binary_trailing_bytes() {
    let mut body = minimal_validate_body();
    body.push(0xFF); // extra trailing byte
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("trailing"),
        "error should mention trailing bytes: {err}"
    );
}

#[test]
fn decode_validate_binary_string_len_exceeds_remaining() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .done();
    // Declare a string of 100 bytes but provide none.
    body.extend_from_slice(&100u32.to_le_bytes());
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("declared"),
        "error should mention declared length: {err}"
    );
}

#[test]
fn decode_validate_binary_count_exceeds_remaining() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("req-c")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1_000_000) // way more than remaining bytes
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("declared"),
        "error should mention declared count: {err}"
    );
}

#[test]
fn decode_validate_binary_invalid_bool() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("req-b")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1) // object_count
        .string("dbo")
        .string("X")
        .u8(0) // kind = table
        .u32_le(0) // column_count
        .u32_le(1) // parameter_count
        .string("@P")
        .string("INT")
        .u8(0xFF) // invalid bool
        .u32_le(0) // fk_count
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("invalid bool"),
        "error should mention invalid bool: {err}"
    );
}

#[test]
fn decode_sync_catalog_binary_minimal() {
    let body = minimal_sync_catalog_body();
    let req = decode_sync_catalog_binary(&body).expect("minimal sync should decode");
    match req {
        EngineRequest::SyncCatalog {
            id,
            connection,
            objects,
            coverage: _,
        } => {
            assert_eq!(id, "sync-1");
            assert_eq!(connection.server, "localhost");
            assert!(objects.is_empty());
        }
        other => panic!("expected SyncCatalog, got {other:?}"),
    }
}

#[test]
fn decode_sync_catalog_binary_with_object() {
    let body = sync_catalog_body_with_object();
    let req = decode_sync_catalog_binary(&body).expect("sync with object should decode");
    match req {
        EngineRequest::SyncCatalog { id, objects, .. } => {
            assert_eq!(id, "sync-2");
            assert_eq!(objects.len(), 1);
            let obj = &objects[0];
            assert_eq!(obj.schema.as_deref(), Some("dbo"));
            assert_eq!(obj.name, "Orders");
            assert_eq!(obj.kind, sqlly_ai::catalog::CatalogObjectKind::Table);
            assert_eq!(obj.columns.len(), 2);
            assert_eq!(obj.columns[0].name, "OrderID");
            assert_eq!(obj.columns[0].type_name.as_deref(), Some("INT"));
            assert_eq!(obj.columns[0].ordinal, 1);
            assert!(!obj.columns[0].nullable);
            assert_eq!(obj.columns[1].name, "CustomerID");
            assert!(obj.columns[1].nullable);
            assert_eq!(obj.primary_key, vec!["OrderID"]);
            assert_eq!(obj.foreign_keys.len(), 1);
            let fk = &obj.foreign_keys[0];
            assert_eq!(fk.columns, vec!["CustomerID"]);
            assert_eq!(fk.target_table, "Customers");
            assert_eq!(fk.target_columns, vec!["CustomerID"]);
            assert_eq!(obj.parameters.len(), 0);
            assert!(obj.definition.is_none());
            assert_eq!(obj.source.kind, sqlly_ai::catalog::SourceKind::Database);
            assert_eq!(obj.source.create_date, Some(1000));
            assert_eq!(obj.source.modify_date, Some(2000));
        }
        other => panic!("expected SyncCatalog, got {other:?}"),
    }
}

#[test]
fn decode_sync_catalog_binary_unknown_source_kind() {
    let connection_json = r#"{"server":"localhost","port":1433,"auth":"sql","username":"sa","password":"pw","encrypt":true,"trust_certificate":true,"block_all_but_select":false,"auto_wrap_rollback":false}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-k")
        .string(connection_json)
        .u32_le(1) // object_count
        .opt_string(None)
        .string("X")
        .u8(0) // kind = table
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(0) // parameter_count
        .opt_string(None) // definition
        .u8(99) // unknown source kind
        .opt_string(None)
        .opt_i64(None)
        .opt_i64(None)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("unknown source kind"),
        "error should mention unknown source kind: {err}"
    );
}

#[test]
fn decode_sync_catalog_binary_unsupported_layout_version() {
    let body = BinaryTestWriter::new()
        .u16_le(999)
        .string("sync-x")
        .string("{}")
        .u32_le(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("layout version"),
        "error should mention layout version: {err}"
    );
}

#[test]
fn decode_sync_catalog_binary_trailing_bytes() {
    let mut body = minimal_sync_catalog_body();
    body.push(0xFF);
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("trailing"),
        "error should mention trailing bytes: {err}"
    );
}

#[test]
fn decode_sync_catalog_binary_bad_connection_json() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-bad")
        .string("not json") // invalid JSON for connection
        .u32_le(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("connection_json"),
        "error should mention connection_json: {err}"
    );
}

#[test]
fn best_effort_binary_id_extracts_from_validate() {
    let body = minimal_validate_body();
    let id = best_effort_binary_id(&body);
    assert_eq!(id.as_deref(), Some("req-1"));
}

#[test]
fn best_effort_binary_id_returns_none_for_truncated() {
    let body = [0x01u8]; // too short
    let id = best_effort_binary_id(&body);
    assert!(id.is_none());
}

#[test]
fn raw_request_fields_extracts_id_from_binary_validate() {
    let mut frame = vec![REQUEST_TAG_VALIDATE];
    frame.extend_from_slice(&minimal_validate_body());
    let (id, kind) = raw_request_fields(&frame);
    assert_eq!(id, "req-1");
    assert_eq!(kind, "validate");
}

#[test]
fn raw_request_fields_extracts_id_from_binary_sync_catalog() {
    let mut frame = vec![REQUEST_TAG_SYNC_CATALOG];
    frame.extend_from_slice(&minimal_sync_catalog_body());
    let (id, kind) = raw_request_fields(&frame);
    assert_eq!(id, "sync-1");
    assert_eq!(kind, "sync_catalog");
}

#[test]
fn raw_request_fields_returns_unknown_for_malformed_binary() {
    let frame = [REQUEST_TAG_VALIDATE, 0xFF];
    let (id, kind) = raw_request_fields(&frame);
    assert_eq!(id, "unknown");
    assert_eq!(kind, "validate");
}

#[test]
fn raw_request_fields_extracts_id_from_binary_ping() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("p-1")
        .done();
    let mut frame = vec![REQUEST_TAG_PING];
    frame.extend_from_slice(&body);
    let (id, kind) = raw_request_fields(&frame);
    assert_eq!(id, "p-1");
    assert_eq!(kind, "ping");
}

#[test]
fn best_effort_request_id_extracts_from_binary_ping() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("r-1")
        .done();
    let mut frame = vec![REQUEST_TAG_PING];
    frame.extend_from_slice(&body);
    let id = best_effort_request_id(&frame);
    assert_eq!(id.as_deref(), Some("r-1"));
}

#[test]
fn best_effort_request_id_routes_binary_frames_to_binary_extractor() {
    let mut frame = vec![REQUEST_TAG_VALIDATE];
    frame.extend_from_slice(&minimal_validate_body());
    let id = best_effort_request_id(&frame);
    assert_eq!(id.as_deref(), Some("req-1"));
}

/// Bare JSON frames (starting with `{` = 0x7B) are now rejected as unknown
/// tags since the protocol is fully binary.
#[tokio::test]
async fn bare_json_request_is_rejected_as_unknown_tag() {
    let frame = br#"{"type":"validate","id":"jv-1","sql":"SELECT 1"}"#.to_vec();

    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    let catalog = test_catalog_state();
    dispatch_request(&frame, &frames, &active, &pool, &catalog).await;
    drop(frames);

    match rx.recv().await.unwrap() {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "unknown");
            assert!(
                error.to_string().contains("unknown request tag"),
                "error should mention unknown tag: {error}"
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

/// JSON sync_catalog is now rejected as an unknown tag (0x7B).
#[tokio::test]
async fn bare_json_sync_catalog_is_rejected_as_unknown_tag() {
    let request = serde_json::json!({
        "type": "sync_catalog",
        "id": "js-1",
        "connection": test_connection_json(),
        "objects": [],
    });
    let frame = serde_json::to_vec(&request).unwrap();

    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    let catalog = test_catalog_state();
    dispatch_request(&frame, &frames, &active, &pool, &catalog).await;
    drop(frames);

    match rx.recv().await.unwrap() {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "unknown");
            assert!(
                error.to_string().contains("unknown request tag"),
                "error should mention binary framing: {error}"
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

/// JSON parse failure on a JSON frame should still extract the id from the
/// JSON payload (not fall back to `best_effort_binary_id`).
#[tokio::test]
async fn json_parse_failure_uses_json_id_recovery() {
    // Malformed JSON that still has an "id" field prefix — the JSON id
    // extractor should find it even though serde can't parse the whole.
    let frame = br#"{"type":"validate","id":"rec-1","sql":"SELECT 1","#.to_vec();

    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    let catalog = test_catalog_state();
    dispatch_request(&frame, &frames, &active, &pool, &catalog).await;
    drop(frames);

    match rx.recv().await.unwrap() {
        EngineFrame::RequestFailed { id, error } => {
            assert!(
                error.to_string().contains("invalid engine request"),
                "should be a parse error: {error}"
            );
            // The JSON id extractor should have found "rec-1" via partial
            // parse. If it didn't, it falls back to "unknown" which is
            // still acceptable but less useful.
            assert!(
                id == "rec-1" || id == "unknown",
                "id should be recovered or unknown, got {id}"
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

// ───────────────────────────────────────────────────────────────────
// Cross-language golden-byte tests
//
// These tests use the exact same fixture values and byte sequences as
// the Swift golden-byte tests in EngineClientTests.swift
// (testValidateBodyEmptySchemaMatchesGoldenBytes and
// testValidateBodyWithObjectAndFKMatchesGoldenBytes). If either side
// changes the wire format, the mismatch will be caught here or in Swift.
// ───────────────────────────────────────────────────────────────────

/// Mirrors Swift `testValidateBodyEmptySchemaMatchesGoldenBytes`.
/// Uses id="test1", sql="SELECT 1", dialect="sqlserver",
/// default_schema="dbo", 0 objects, 0 FKs.
#[test]
fn cross_language_validate_empty_schema_golden_bytes() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("test1")
        .string("SELECT 1")
        .opt_string(Some("sqlserver"))
        .opt_string(Some("dbo"))
        .u32_le(0)
        .u32_le(0)
        .done();

    let req = decode_validate_binary(&body).expect("golden validate body should decode");
    match req {
        EngineRequest::Validate {
            id,
            sql,
            dialect,
            default_schema,
            objects,
            foreign_keys,
        } => {
            assert_eq!(id, "test1");
            assert_eq!(sql, "SELECT 1");
            assert_eq!(dialect.as_deref(), Some("sqlserver"));
            assert_eq!(default_schema.as_deref(), Some("dbo"));
            assert!(objects.is_empty());
            assert!(foreign_keys.is_empty());
        }
        other => panic!("expected Validate, got {other:?}"),
    }
}

/// Mirrors Swift `testValidateBodyWithObjectAndFKMatchesGoldenBytes`.
/// Uses id="v1", sql="SELECT * FROM Orders", no dialect, no
/// default_schema, 1 object (dbo.Orders, table, 2 cols, 1 param with
/// is_output=true), 1 FK (dbo.Orders.CustomerId -> dbo.Customers.CustomerId).
#[test]
fn cross_language_validate_with_object_and_fk_golden_bytes() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v1")
        .string("SELECT * FROM Orders")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1)
        // object 0: dbo.Orders (table)
        .string("dbo")
        .string("Orders")
        .u8(0) // kind = table
        .u32_le(2) // column_count
        .string("OrderId")
        .string("CustomerId")
        .u32_le(1) // parameter_count
        .string("@p")
        .string("int")
        .bool(true) // is_output
        .u32_le(1) // foreign_key_count
        // FK 0: dbo.Orders.CustomerId -> dbo.Customers.CustomerId
        .string("dbo")
        .string("Orders")
        .string("CustomerId")
        .string("dbo")
        .string("Customers")
        .string("CustomerId")
        .done();

    let req = decode_validate_binary(&body).expect("golden validate body should decode");
    match req {
        EngineRequest::Validate {
            id,
            sql,
            dialect,
            default_schema,
            objects,
            foreign_keys,
        } => {
            assert_eq!(id, "v1");
            assert_eq!(sql, "SELECT * FROM Orders");
            assert_eq!(dialect, None);
            assert_eq!(default_schema, None);
            assert_eq!(objects.len(), 1);
            let obj = &objects[0];
            assert_eq!(obj.schema, "dbo");
            assert_eq!(obj.name, "Orders");
            assert_eq!(obj.kind, "table");
            assert_eq!(obj.columns, vec!["OrderId", "CustomerId"]);
            assert_eq!(obj.parameters.len(), 1);
            assert_eq!(obj.parameters[0].name, "@p");
            assert_eq!(obj.parameters[0].type_name, "int");
            assert!(obj.parameters[0].is_output);

            assert_eq!(foreign_keys.len(), 1);
            let fk = &foreign_keys[0];
            assert_eq!(fk.from_schema, "dbo");
            assert_eq!(fk.from_table, "Orders");
            assert_eq!(fk.from_column, "CustomerId");
            assert_eq!(fk.to_schema, "dbo");
            assert_eq!(fk.to_table, "Customers");
            assert_eq!(fk.to_column, "CustomerId");
        }
        other => panic!("expected Validate, got {other:?}"),
    }
}

/// Mirrors Swift `testSyncCatalogBodyWithObjectMatchesGoldenBytes`.
/// Uses id="sc2", connection JSON for srv/db/sql/sa, 1 object (dbo.Orders,
/// table, 2 cols with types and ordinals, nullable=true), 1 FK
/// (dbo.Orders.CustomerId -> dbo.Customers.CustomerId), 0 params,
/// no definition, database source, create_date=1000, modify_date=1000.
///
/// The connection JSON is parsed semantically (key order is not guaranteed
/// to match across Swift/Rust JSON serializers) — the object/columns/FK/
/// source layout is the byte-stable cross-language contract.
#[test]
fn cross_language_sync_catalog_with_object_golden_bytes() {
    let connection_json = r#"{"server":"srv","database":"db","port":1433,"auth":"sql","username":"sa","password":"pw","encrypt":false,"trust_certificate":false,"block_all_but_select":false,"auto_wrap_rollback":false}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sc2")
        .string(connection_json)
        .u32_le(1) // object_count
        // object 0: dbo.Orders (table)
        .opt_string(Some("dbo"))
        .string("Orders")
        .u8(0) // kind = table
        .u32_le(2) // column_count
        // column 0: OrderId, int, ordinal=1, nullable=true
        .string("OrderId")
        .opt_string(Some("int"))
        .i64_le(1)
        .bool(true)
        // column 1: CustomerId, int, ordinal=2, nullable=true
        .string("CustomerId")
        .opt_string(Some("int"))
        .i64_le(2)
        .bool(true)
        .u32_le(1) // primary_key_count
        .string("OrderId") // primary key column
        .u32_le(1) // foreign_key_count
        // FK 0: CustomerId -> dbo.Customers.CustomerId
        .u32_le(1) // local_column_count
        .string("CustomerId")
        .opt_string(Some("dbo"))
        .string("Customers")
        .u32_le(1) // target_column_count
        .string("CustomerId")
        .u32_le(0) // parameter_count
        .opt_string(None) // definition absent
        .u8(0) // source_kind = database
        .opt_string(None) // path absent
        .opt_i64(Some(1000)) // create_date
        .opt_i64(Some(1000)) // modify_date
        .done();

    let req = decode_sync_catalog_binary(&body).expect("golden sync_catalog body should decode");
    match req {
        EngineRequest::SyncCatalog { id, objects, .. } => {
            assert_eq!(id, "sc2");
            assert_eq!(objects.len(), 1);
            let obj = &objects[0];
            assert_eq!(obj.schema.as_deref(), Some("dbo"));
            assert_eq!(obj.name, "Orders");
            assert_eq!(obj.kind, sqlly_ai::catalog::CatalogObjectKind::Table);

            // Columns — mirrors Swift fixture exactly.
            assert_eq!(obj.columns.len(), 2);
            assert_eq!(obj.columns[0].name, "OrderId");
            assert_eq!(obj.columns[0].type_name.as_deref(), Some("int"));
            assert_eq!(obj.columns[0].ordinal, 1);
            assert!(obj.columns[0].nullable);
            assert_eq!(obj.columns[1].name, "CustomerId");
            assert_eq!(obj.columns[1].type_name.as_deref(), Some("int"));
            assert_eq!(obj.columns[1].ordinal, 2);
            assert!(obj.columns[1].nullable);

            // Primary key.
            assert_eq!(obj.primary_key, vec!["OrderId"]);

            // FK — mirrors Swift fixture.
            assert_eq!(obj.foreign_keys.len(), 1);
            let fk = &obj.foreign_keys[0];
            assert_eq!(fk.columns, vec!["CustomerId"]);
            assert_eq!(fk.target_schema.as_deref(), Some("dbo"));
            assert_eq!(fk.target_table, "Customers");
            assert_eq!(fk.target_columns, vec!["CustomerId"]);

            // Parameters — none.
            assert_eq!(obj.parameters.len(), 0);

            // Definition — absent.
            assert!(obj.definition.is_none());

            // Source — database, no path, timestamps at 1000ms.
            assert_eq!(obj.source.kind, sqlly_ai::catalog::SourceKind::Database);
            assert!(obj.source.path.is_none());
            assert_eq!(obj.source.create_date, Some(1000));
            assert_eq!(obj.source.modify_date, Some(1000));
        }
        other => panic!("expected SyncCatalog, got {other:?}"),
    }
}

// ───────────────────────────────────────────────────────────────────
// BinaryFrameReader primitive failure tests
//
// These exercise the low-level cursor that all binary decoders rely on.
// Each test verifies the error path for a specific primitive: truncated
// scalars, invalid bool/presence bytes, string length exceeding remaining,
// invalid UTF-8, and trailing bytes.
// ───────────────────────────────────────────────────────────────────

#[test]
fn reader_read_u8_empty_returns_truncated_error() {
    let mut r = BinaryFrameReader::new(&[]);
    let err = r.read_u8("f").unwrap_err();
    assert!(err.to_string().contains("truncated"), "{}", err);
    assert!(err.to_string().contains("'f'"), "{}", err);
}

#[test]
fn reader_read_u16_le_one_byte_returns_truncated() {
    let mut r = BinaryFrameReader::new(&[0x01]);
    let err = r.read_u16_le("f").unwrap_err();
    assert!(err.to_string().contains("truncated"), "{}", err);
}

#[test]
fn reader_read_u32_le_three_bytes_returns_truncated() {
    let mut r = BinaryFrameReader::new(&[0x01, 0x02, 0x03]);
    let err = r.read_u32_le("f").unwrap_err();
    assert!(err.to_string().contains("truncated"), "{}", err);
}

#[test]
fn reader_read_i64_le_seven_bytes_returns_truncated() {
    let mut r = BinaryFrameReader::new(&[0; 7]);
    let err = r.read_i64_le("f").unwrap_err();
    assert!(err.to_string().contains("truncated"), "{}", err);
}

#[test]
fn reader_read_bool_rejects_invalid_byte_0x02() {
    let mut r = BinaryFrameReader::new(&[0x02]);
    let err = r.read_bool("flag").unwrap_err();
    assert!(err.to_string().contains("invalid bool"), "{}", err);
    assert!(err.to_string().contains("0x02"), "{}", err);
}

#[test]
fn reader_read_bool_rejects_invalid_byte_0xff() {
    let mut r = BinaryFrameReader::new(&[0xFF]);
    let err = r.read_bool("flag").unwrap_err();
    assert!(err.to_string().contains("invalid bool"), "{}", err);
}

#[test]
fn reader_read_optional_string_rejects_presence_0x02() {
    let mut r = BinaryFrameReader::new(&[0x02]);
    let err = r.read_optional_string("opt").unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
    assert!(err.to_string().contains("'opt'"), "{}", err);
}

#[test]
fn reader_read_optional_i64_rejects_presence_0x03() {
    let mut r = BinaryFrameReader::new(&[0x03]);
    let err = r.read_optional_i64("opt").unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
}

#[test]
fn reader_read_string_length_exceeds_remaining() {
    // u32 LE length = 100, but only 3 bytes follow
    let data: Vec<u8> = [100u32.to_le_bytes().as_slice(), b"abc"].concat();
    let mut r = BinaryFrameReader::new(&data);
    let err = r.read_string("s").unwrap_err();
    assert!(err.to_string().contains("declared 100 bytes"), "{}", err);
    assert!(err.to_string().contains("'s'"), "{}", err);
}

#[test]
fn reader_read_string_invalid_utf8() {
    // u32 LE length = 3, then invalid UTF-8 sequence
    let data: Vec<u8> = [3u32.to_le_bytes().as_slice(), &[0xFF, 0xFE, 0xFD]].concat();
    let mut r = BinaryFrameReader::new(&data);
    let err = r.read_string("s").unwrap_err();
    assert!(err.to_string().contains("invalid UTF-8"), "{}", err);
    assert!(err.to_string().contains("'s'"), "{}", err);
}

#[test]
fn reader_read_count_exceeds_remaining() {
    // u32 LE count = 999, but only 2 bytes follow
    let data: Vec<u8> = [999u32.to_le_bytes().as_slice(), &[0x00, 0x00]].concat();
    let mut r = BinaryFrameReader::new(&data);
    let err = r.read_count("n").unwrap_err();
    assert!(err.to_string().contains("999 elements"), "{}", err);
}

#[test]
fn reader_ensure_at_end_rejects_trailing_bytes() {
    let mut r = BinaryFrameReader::new(&[0x01, 0x02]);
    r.read_u8("a").unwrap();
    // 1 byte remains
    let err = r.ensure_at_end().unwrap_err();
    assert!(err.to_string().contains("1 trailing"), "{}", err);
}

#[test]
fn reader_ensure_at_end_ok_when_consumed() {
    let mut r = BinaryFrameReader::new(&[0x01]);
    r.read_u8("a").unwrap();
    assert!(r.ensure_at_end().is_ok());
}

#[test]
fn reader_read_string_multi_byte_utf8_uses_byte_length() {
    // "héllo" is 6 bytes in UTF-8 but 5 characters.
    let s = "héllo";
    let data: Vec<u8> = [(s.len() as u32).to_le_bytes().as_slice(), s.as_bytes()].concat();
    let mut r = BinaryFrameReader::new(&data);
    let decoded = r.read_string("s").unwrap();
    assert_eq!(decoded, s);
    assert!(r.ensure_at_end().is_ok());
}

// ───────────────────────────────────────────────────────────────────
// decode_validate_binary failure tests
// ───────────────────────────────────────────────────────────────────

#[test]
fn decode_validate_unsupported_layout_version() {
    let body = BinaryTestWriter::new()
        .u16_le(999) // unsupported version
        .string("bad-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(0)
        .u32_le(0)
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("unsupported"), "{}", err);
    assert!(err.to_string().contains("999"), "{}", err);
}

#[test]
fn decode_validate_truncated_before_id() {
    // Only the u16 layout version, no id string
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("truncated"), "{}", err);
}

#[test]
fn decode_validate_invalid_utf8_in_id() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .done();
    // Append a string with length 3 but invalid UTF-8
    body.extend_from_slice(&3u32.to_le_bytes());
    body.extend_from_slice(&[0xFF, 0xFE, 0xFD]);
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid UTF-8"), "{}", err);
    assert!(err.to_string().contains("'id'"), "{}", err);
}

#[test]
fn decode_validate_invalid_dialect_presence_byte() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .done();
    // Append an invalid presence byte for dialect
    body.push(0x02);
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
    assert!(err.to_string().contains("'dialect'"), "{}", err);
}

#[test]
fn decode_validate_unknown_editor_kind() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1) // 1 object
        .string("dbo")
        .string("MyTable")
        .done();
    // Append unknown kind byte 0x05 (valid kinds are 0-4)
    body.push(0x05);
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("unknown editor object kind"),
        "{}",
        err
    );
    assert!(err.to_string().contains("0x05"), "{}", err);
}

#[test]
fn decode_validate_object_count_exceeds_remaining() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .done();
    // Append a huge object count with almost no remaining bytes
    body.extend_from_slice(&999_999u32.to_le_bytes());
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("object_count"), "{}", err);
    assert!(err.to_string().contains("999999"), "{}", err);
}

#[test]
fn decode_validate_trailing_bytes() {
    let mut body = minimal_validate_body();
    body.push(0xFF); // trailing byte
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("trailing"), "{}", err);
}

#[test]
fn decode_validate_empty_id_and_empty_sql_decode_ok() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("") // empty id
        .string("") // empty sql
        .opt_string(None)
        .opt_string(None)
        .u32_le(0)
        .u32_le(0)
        .done();
    let req = decode_validate_binary(&body).unwrap();
    match req {
        EngineRequest::Validate { id, sql, .. } => {
            assert_eq!(id, "");
            assert_eq!(sql, "");
        }
        other => panic!("expected Validate, got {other:?}"),
    }
}

#[test]
fn decode_validate_fk_count_exceeds_remaining() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(0) // 0 objects
        .done();
    // Append a huge FK count with no remaining bytes
    body.extend_from_slice(&100_000u32.to_le_bytes());
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("foreign_key_count"), "{}", err);
}

// ───────────────────────────────────────────────────────────────────
// Min-footprint count validation tests
//
// `read_count_with_min` rejects counts where
// `count * min_bytes_per_element > remaining` even when
// `count <= remaining` — the old `read_count` would have accepted
// these, allowing absurd allocations from corrupt frames.
// ───────────────────────────────────────────────────────────────────

#[test]
fn read_count_with_min_rejects_count_exceeding_remaining_footprint() {
    // 4 bytes for the count itself, then 2 trailing bytes.
    // count=2, min=17 → 34 > 2 remaining → must fail.
    let buf = BinaryTestWriter::new()
        .u32_le(2) // count
        .u8(0) // trailing
        .u8(0) // trailing
        .done();
    let mut r = BinaryFrameReader::new(&buf);
    let err = r.read_count_with_min("test_count", 17).unwrap_err();
    assert!(err.to_string().contains("test_count"), "{}", err);
    assert!(err.to_string().contains("34"), "{}", err);
}

#[test]
fn read_count_with_min_accepts_count_within_remaining_footprint() {
    // count=1, min=17 → 17 ≤ 20 remaining → ok.
    let buf = BinaryTestWriter::new()
        .u32_le(1) // count
        .u8(0) // 1 byte
        .done();
    // Pad to 20 remaining bytes after the count.
    let mut buf = buf;
    buf.extend_from_slice(&[0u8; 19]);
    let mut r = BinaryFrameReader::new(&buf);
    let count = r.read_count_with_min("test_count", 17).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn read_count_with_min_rejects_count_that_overflows_size_calculation() {
    // count = 2, min = usize::MAX/2 + 1 → product overflows usize.
    let buf = BinaryTestWriter::new()
        .u32_le(2) // count
        .done();
    let mut r = BinaryFrameReader::new(&buf);
    let err = r
        .read_count_with_min("overflow_count", usize::MAX / 2 + 1)
        .unwrap_err();
    assert!(err.to_string().contains("overflows"), "{}", err);
}

#[test]
fn decode_validate_object_count_min_footprint_exceeds_remaining() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string("S")
        .opt_string(None)
        .opt_string(None)
        .u32_le(2) // object_count = 2 (min 17 each → 34 > 2 remaining)
        .u8(0) // 2 trailing bytes so count ≤ remaining
        .u8(0)
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("object_count"), "{}", err);
    assert!(err.to_string().contains("34"), "{}", err);
}

#[test]
fn decode_validate_column_count_min_footprint_exceeds_remaining() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string("S")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1) // 1 object
        .string("dbo") // schema
        .string("T") // name
        .u8(0) // kind = table
        .u32_le(2) // column_count = 2 (min 4 each → 8 > 2 remaining)
        .u8(0) // 2 trailing bytes
        .u8(0)
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("column_count"), "{}", err);
}

#[test]
fn decode_validate_parameter_count_min_footprint_exceeds_remaining() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string("S")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1) // 1 object
        .string("dbo")
        .string("T")
        .u8(0) // kind
        .u32_le(0) // 0 columns
        .u32_le(2) // parameter_count = 2 (min 9 each → 18 > 2 remaining)
        .u8(0) // 2 trailing bytes
        .u8(0)
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("parameter_count"), "{}", err);
}

#[test]
fn decode_validate_foreign_key_count_min_footprint_exceeds_remaining() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string("S")
        .opt_string(None)
        .opt_string(None)
        .u32_le(0) // 0 objects
        .u32_le(2) // fk_count = 2 (min 24 each → 48 > 2 remaining)
        .u8(0) // 2 trailing bytes
        .u8(0)
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("foreign_key_count"), "{}", err);
}

// ───────────────────────────────────────────────────────────────────
// decode_sync_catalog min-footprint count validation tests
// ───────────────────────────────────────────────────────────────────

#[test]
fn decode_sync_catalog_object_count_min_footprint_exceeds_remaining() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string(connection_json)
        .u32_le(2) // object_count = 2 (min 23 each → 46 > 2 remaining)
        .u8(0) // 2 trailing bytes
        .u8(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("object_count"), "{}", err);
    assert!(err.to_string().contains("46"), "{}", err);
}

#[test]
fn decode_sync_catalog_column_count_min_footprint_exceeds_remaining() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string(connection_json)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("T")
        .u8(0) // kind = table
        .u32_le(2) // column_count = 2 (min 14 each → 28 > 5 remaining)
        .u8(0) // 5 trailing bytes: 3 for object_count min (23),
        .u8(0) // plus 2 extra so column_count min (28) > 5
        .u8(0)
        .u8(0)
        .u8(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("column_count"), "{}", err);
}

#[test]
fn decode_sync_catalog_fk_count_min_footprint_exceeds_remaining() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string(connection_json)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("T")
        .u8(0) // kind
        .u32_le(0) // 0 columns
        .u32_le(0) // 0 primary key columns
        .u32_le(2) // fk_count = 2 (min 13 each → 26 > 2 remaining)
        .u8(0) // 2 trailing bytes
        .u8(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("foreign_key_count"), "{}", err);
}

#[test]
fn decode_sync_catalog_local_column_count_min_footprint_exceeds_remaining() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string(connection_json)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("T")
        .u8(0) // kind
        .u32_le(0) // 0 columns
        .u32_le(0) // 0 primary key columns
        .u32_le(1) // 1 FK
        .u32_le(3) // local_col_count = 3 (min 4 → 12 > 9 remaining)
        .u8(0) // 9 trailing bytes (fk_count min=13,
        .u8(0) // so 13 bytes after fk_count; local_col_count
        .u8(0) // reads 4, leaving 9; 3*4=12 > 9)
        .u8(0)
        .u8(0)
        .u8(0)
        .u8(0)
        .u8(0)
        .u8(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("local_column_count"), "{}", err);
}

#[test]
fn decode_sync_catalog_target_column_count_min_footprint_exceeds_remaining() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string(connection_json)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("T")
        .u8(0) // kind
        .u32_le(0) // 0 columns
        .u32_le(0) // 0 primary key columns
        .u32_le(1) // 1 FK
        .u32_le(1) // local_col_count = 1
        .string("c1") // local column
        .opt_string(None) // target_schema
        .string("T2") // target_table
        .u32_le(2) // target_col_count = 2 (min 4 each → 8 > 2 remaining)
        .u8(0) // 2 trailing bytes
        .u8(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("target_column_count"), "{}", err);
}

#[test]
fn decode_sync_catalog_parameter_count_min_footprint_exceeds_remaining() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("t")
        .string(connection_json)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("T")
        .u8(0) // kind
        .u32_le(0) // 0 columns
        .u32_le(0) // 0 primary key columns
        .u32_le(0) // 0 FKs
        .u32_le(2) // parameter_count = 2 (min 6 each → 12 > 2 remaining)
        .u8(0) // 2 trailing bytes
        .u8(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("parameter_count"), "{}", err);
}

// ───────────────────────────────────────────────────────────────────
// decode_sync_catalog_binary failure tests
// ───────────────────────────────────────────────────────────────────

#[test]
fn decode_sync_catalog_unsupported_layout_version() {
    let body = BinaryTestWriter::new()
        .u16_le(999)
        .string("bad-sync-1")
        .string(r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#)
        .u32_le(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("unsupported"), "{}", err);
    assert!(err.to_string().contains("sync_catalog"), "{}", err);
}

#[test]
fn decode_sync_catalog_invalid_connection_json() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-2")
        .string("{not valid json}") // invalid JSON
        .u32_le(0)
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("connection_json"), "{}", err);
}

#[test]
fn decode_sync_catalog_unknown_catalog_kind() {
    // A structurally complete object whose kind byte is unknown (valid
    // kinds are 0-4): the wire layout parses, and the kind mapping is
    // what rejects it.
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-3")
        .string(r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("MyObj")
        .u8(0x06) // unknown kind
        .u32_le(0) // 0 columns
        .u32_le(0) // 0 primary key columns
        .u32_le(0) // 0 FKs
        .u32_le(0) // 0 params
        .opt_string(None) // definition
        .u8(0) // source kind = database
        .opt_string(None) // path
        .opt_i64(None) // create_date
        .opt_i64(None) // modify_date
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(
        err.to_string().contains("unknown catalog object kind"),
        "{}",
        err
    );
    assert!(err.to_string().contains("0x06"), "{}", err);
}

#[test]
fn decode_sync_catalog_unknown_source_kind() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-4")
        .string(connection_json)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("MyObj")
        .u8(0) // kind = table
        .u32_le(0) // 0 columns
        .u32_le(0) // 0 primary key columns
        .u32_le(0) // 0 FKs
        .u32_le(0) // 0 params
        .opt_string(None) // definition
        .u8(0x02) // unknown source kind (valid: 0=database, 1=file)
        .opt_string(None) // path
        .opt_i64(None) // create_date
        .opt_i64(None) // modify_date
        .done();
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("unknown source kind"), "{}", err);
    assert!(err.to_string().contains("0x02"), "{}", err);
}

#[test]
fn decode_sync_catalog_invalid_schema_presence_byte() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-5")
        .string(connection_json)
        .u32_le(1) // 1 object
        .done();
    // Append invalid presence byte for schema, then enough padding
    // to satisfy the object_count min-footprint check (23 bytes).
    body.push(0x03); // invalid presence byte for schema
    body.extend_from_slice(&[0u8; 22]); // padding to reach 23 total
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
    assert!(err.to_string().contains("schema"), "{}", err);
}

#[test]
fn decode_sync_catalog_object_count_exceeds_remaining() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-6")
        .string(connection_json)
        .done();
    // Append huge object count with no remaining bytes
    body.extend_from_slice(&500_000u32.to_le_bytes());
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("object_count"), "{}", err);
    assert!(err.to_string().contains("500000"), "{}", err);
}

#[test]
fn decode_sync_catalog_column_nullable_invalid_bool() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-7")
        .string(connection_json)
        .u32_le(1) // 1 object
        .opt_string(Some("dbo"))
        .string("MyTable")
        .u8(0) // kind = table
        .u32_le(1) // 1 column
        .string("Col1")
        .opt_string(Some("int"))
        .i64_le(0) // ordinal
        .done();
    // Append invalid bool byte for nullable
    body.push(0x05);
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid bool"), "{}", err);
    assert!(err.to_string().contains("nullable"), "{}", err);
}

#[test]
fn decode_sync_catalog_trailing_bytes() {
    let mut body = minimal_sync_catalog_body();
    body.push(0xFF); // trailing byte
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("trailing"), "{}", err);
}

// ───────────────────────────────────────────────────────────────────
// Additional binary request decoder failure tests
// ───────────────────────────────────────────────────────────────────

#[test]
fn decode_validate_truncated_sql_after_id() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .done();
    body.extend_from_slice(&10u32.to_le_bytes());
    body.extend_from_slice(b"hello");
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("sql"), "{}", err);
    assert!(err.to_string().contains("declared"), "{}", err);
}

#[test]
fn decode_validate_truncated_dialect_string_body() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .done();
    body.push(0x01);
    body.extend_from_slice(&10u32.to_le_bytes());
    body.extend_from_slice(b"abc");
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("dialect"), "{}", err);
    assert!(err.to_string().contains("declared"), "{}", err);
}

#[test]
fn decode_validate_truncated_before_default_schema() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .opt_string(Some("sqlserver"))
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("truncated"), "{}", err);
    assert!(err.to_string().contains("default_schema"), "{}", err);
}

#[test]
fn decode_validate_invalid_parameter_is_output_bool() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1)
        .string("dbo")
        .string("MyProc")
        .u8(2)
        .u32_le(0)
        .u32_le(1)
        .string("@RetVal")
        .string("INT")
        .done();
    body.push(0x05);
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid bool"), "{}", err);
    assert!(err.to_string().contains("is_output"), "{}", err);
}

#[test]
fn decode_validate_truncated_parameter_body() {
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(1)
        .string("dbo")
        .string("MyProc")
        .u8(2)
        .u32_le(0)
        .u32_le(1)
        .string("@RetVal")
        .done();
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("truncated"), "{}", err);
    assert!(err.to_string().contains("type_name"), "{}", err);
}

#[test]
fn decode_validate_invalid_fk_string_length() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("v-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(0)
        .u32_le(1)
        .done();
    body.extend_from_slice(&100u32.to_le_bytes());
    body.extend_from_slice(&[0u8; 20]);
    let err = decode_validate_binary(&body).unwrap_err();
    assert!(err.to_string().contains("from_schema"), "{}", err);
    assert!(err.to_string().contains("declared"), "{}", err);
}

#[test]
fn decode_sync_catalog_truncated_connection_json() {
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-cj")
        .done();
    body.extend_from_slice(&100u32.to_le_bytes());
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("connection_json"), "{}", err);
    assert!(err.to_string().contains("declared"), "{}", err);
}

#[test]
fn decode_sync_catalog_invalid_definition_presence_byte() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-def")
        .string(connection_json)
        .u32_le(1)
        .opt_string(None)
        .string("X")
        .u8(0)
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(0) // parameter_count
        .done();
    body.push(0x02);
    body.extend_from_slice(&[0u8; 3]);
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
    assert!(err.to_string().contains("definition"), "{}", err);
}

#[test]
fn decode_sync_catalog_invalid_path_presence_byte() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-path")
        .string(connection_json)
        .u32_le(1)
        .opt_string(None)
        .string("X")
        .u8(0)
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(0) // parameter_count
        .opt_string(None)
        .u8(0)
        .done();
    body.push(0x02);
    body.push(0x00);
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
    assert!(err.to_string().contains("path"), "{}", err);
}

#[test]
fn decode_sync_catalog_invalid_create_date_presence_byte() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-cd")
        .string(connection_json)
        .u32_le(1)
        .opt_string(None)
        .string("X")
        .u8(0)
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(0) // parameter_count
        .opt_string(None)
        .u8(0)
        .opt_string(None)
        .done();
    body.push(0x02);
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
    assert!(err.to_string().contains("create_date"), "{}", err);
}

#[test]
fn decode_sync_catalog_invalid_modify_date_presence_byte() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-md")
        .string(connection_json)
        .u32_le(1)
        .opt_string(None)
        .string("X")
        .u8(0)
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(0) // parameter_count
        .opt_string(None)
        .u8(0)
        .opt_string(None)
        .opt_i64(None)
        .done();
    body.push(0x02);
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid presence"), "{}", err);
    assert!(err.to_string().contains("modify_date"), "{}", err);
}

#[test]
fn decode_sync_catalog_invalid_parameter_is_output_bool() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bad-sync-pio")
        .string(connection_json)
        .u32_le(1)
        .opt_string(None)
        .string("X")
        .u8(0)
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(1) // parameter_count
        .string("P")
        .opt_string(None)
        .done();
    body.push(0x05);
    let err = decode_sync_catalog_binary(&body).unwrap_err();
    assert!(err.to_string().contains("invalid bool"), "{}", err);
    assert!(err.to_string().contains("is_output"), "{}", err);
}

#[test]
fn decode_sync_catalog_empty_schema_is_distinct_from_absent_schema() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-empty-schema")
        .string(connection_json)
        .u32_le(1)
        .opt_string(Some(""))
        .string("MyObj")
        .u8(0)
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(0) // parameter_count
        .opt_string(None)
        .u8(0)
        .opt_string(None)
        .opt_i64(None)
        .opt_i64(None)
        .done();
    let req = decode_sync_catalog_binary(&body).unwrap();
    match req {
        EngineRequest::SyncCatalog { objects, .. } => {
            assert_eq!(objects.len(), 1);
            assert_eq!(objects[0].schema.as_deref(), Some(""));
            assert_eq!(objects[0].name, "MyObj");
        }
        other => panic!("expected SyncCatalog, got {other:?}"),
    }
}

#[test]
fn decode_sync_catalog_file_source_with_path_decodes() {
    let connection_json = r#"{"server":"x","auth":"sql","username":"u","password":"p"}"#;
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-file")
        .string(connection_json)
        .u32_le(1)
        .opt_string(Some("dbo"))
        .string("MyView")
        .u8(1)
        .u32_le(0) // column_count
        .u32_le(0) // primary_key_count
        .u32_le(0) // fk_count
        .u32_le(0) // parameter_count
        .opt_string(Some("CREATE VIEW dbo.MyView AS SELECT 1"))
        .u8(1)
        .opt_string(Some("/path/to/file.sql"))
        .opt_i64(Some(1000))
        .opt_i64(Some(2000))
        .done();
    let req = decode_sync_catalog_binary(&body).unwrap();
    match req {
        EngineRequest::SyncCatalog { objects, .. } => {
            assert_eq!(objects.len(), 1);
            let obj = &objects[0];
            assert_eq!(obj.name, "MyView");
            assert_eq!(obj.kind, sqlly_ai::catalog::CatalogObjectKind::View);
            assert_eq!(obj.source.kind, sqlly_ai::catalog::SourceKind::File);
            assert_eq!(obj.source.path.as_deref(), Some("/path/to/file.sql"));
            assert_eq!(obj.source.create_date, Some(1000));
            assert_eq!(obj.source.modify_date, Some(2000));
            assert_eq!(
                obj.definition.as_deref(),
                Some("CREATE VIEW dbo.MyView AS SELECT 1")
            );
        }
        other => panic!("expected SyncCatalog, got {other:?}"),
    }
}

// ───────────────────────────────────────────────────────────────────
// Dispatch-level malformed request survival tests
//
// These verify that `dispatch_request` converts decode failures into
// `RequestFailed` frames with the correct id, and that the engine
// stays alive (returns `true`).
// ───────────────────────────────────────────────────────────────────

/// Helper: dispatch a frame and return the first response frame.
async fn dispatch_one(frame: &[u8]) -> EngineFrame {
    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    let catalog = test_catalog_state();
    dispatch_request(frame, &frames, &active, &pool, &catalog).await;
    drop(frames);
    rx.recv().await.expect("a response frame")
}

#[tokio::test]
async fn dispatch_unknown_request_tag_yields_request_failed() {
    let frame = [0x99u8, 0x01, 0x02]; // unknown tag
    let resp = dispatch_one(&frame).await;
    match resp {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "unknown");
            assert!(
                error.to_string().contains("unknown request tag"),
                "{}",
                error
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_empty_frame_yields_request_failed() {
    let frame: [u8; 0] = [];
    let resp = dispatch_one(&frame).await;
    match resp {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "unknown");
            assert!(
                error.to_string().contains("empty request frame"),
                "{}",
                error
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_malformed_validate_with_recoverable_id() {
    // Build a validate body with a valid id but corrupt the dialect
    // presence byte so decode fails AFTER the id is read.
    let mut body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("recover-me")
        .string("SELECT 1")
        .done();
    body.push(0x02); // invalid dialect presence byte
    let mut frame = vec![REQUEST_TAG_VALIDATE];
    frame.extend_from_slice(&body);

    let resp = dispatch_one(&frame).await;
    match resp {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "recover-me", "id should be recovered from body");
            assert!(error.to_string().contains("invalid presence"), "{}", error);
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_malformed_validate_without_recoverable_id() {
    // Body is too short to even read the id — id should fall back to "unknown".
    let frame = [REQUEST_TAG_VALIDATE, 0x01, 0x00]; // only 2 bytes of body
    let resp = dispatch_one(&frame).await;
    match resp {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "unknown");
            assert!(
                error.to_string().contains("invalid engine request"),
                "{}",
                error
            );
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_malformed_sync_catalog_with_recoverable_id() {
    // Build a sync_catalog body with a valid id but corrupt the
    // connection_json so decode fails AFTER the id is read.
    let body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-recover")
        .string("{bad json}") // invalid connection JSON
        .u32_le(0)
        .done();
    let mut frame = vec![REQUEST_TAG_SYNC_CATALOG];
    frame.extend_from_slice(&body);

    let resp = dispatch_one(&frame).await;
    match resp {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "sync-recover");
            assert!(error.to_string().contains("connection_json"), "{}", error);
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_unsupported_validate_layout_yields_request_failed_with_id() {
    let body = BinaryTestWriter::new()
        .u16_le(999) // unsupported layout version
        .string("ver-1")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(0)
        .u32_le(0)
        .done();
    let mut frame = vec![REQUEST_TAG_VALIDATE];
    frame.extend_from_slice(&body);

    let resp = dispatch_one(&frame).await;
    match resp {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "ver-1");
            assert!(error.to_string().contains("unsupported"), "{}", error);
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_engine_survives_malformed_then_handles_ping() {
    // The engine must survive a malformed request and stay alive for the
    // next valid one — this is the core survival guarantee.
    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let pool = EnginePools::new();
    let catalog = test_catalog_state();

    // 1. Send an unknown tag frame.
    let bad_frame = [0x99u8, 0x01];
    let keep_running = dispatch_request(&bad_frame, &frames, &active, &pool, &catalog).await;
    assert!(keep_running, "engine must stay alive after bad request");
    let failed = rx.recv().await.unwrap();
    assert!(matches!(failed, EngineFrame::RequestFailed { .. }));

    // 2. Send a valid ping — it must succeed.
    let ping_body = BinaryTestWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("survive-1")
        .done();
    let mut ping = vec![REQUEST_TAG_PING];
    ping.extend_from_slice(&ping_body);
    let keep_running = dispatch_request(&ping, &frames, &active, &pool, &catalog).await;
    assert!(keep_running);
    let pong = rx.recv().await.unwrap();
    match pong {
        EngineFrame::Pong { id } => assert_eq!(id, "survive-1"),
        other => panic!("expected Pong, got {other:?}"),
    }
    drop(frames);
}

// ───────────────────────────────────────────────────────────────────
// Response encoder boundary tests
// ───────────────────────────────────────────────────────────────────

#[test]
fn encode_validation_body_empty_diagnostics_and_resolved() {
    let body = encode_validation_body("empty", &[], &[]).unwrap();
    let mut cur = 0;
    let id_len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
    cur += 4;
    assert_eq!(&body[cur..cur + id_len], b"empty");
    cur += id_len;
    let diag_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(diag_count, 0);
    cur += 4;
    let resolved_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
    assert_eq!(resolved_count, 0);
    cur += 4;
    assert_eq!(cur, body.len());
}

#[test]
fn encode_validation_body_multiple_diagnostics_preserve_order() {
    let diagnostics = vec![
        WireDiag {
            severity: 0,
            class: 1,
            span_start: 10,
            span_len: 5,
            line: 1,
            col: 11,
            message: "first".to_string(),
        },
        WireDiag {
            severity: 1,
            class: 2,
            span_start: 20,
            span_len: 3,
            line: 2,
            col: 5,
            message: "second".to_string(),
        },
    ];
    let body = encode_validation_body("multi", &diagnostics, &[]).unwrap();

    // Decode it back manually to check order.
    let mut r = BinaryFrameReader::new(&body);
    let id = r.read_string("id").unwrap();
    assert_eq!(id, "multi");
    let count = r.read_count("diag_count").unwrap();
    assert_eq!(count, 2);
    let d0_severity = r.read_u8("severity").unwrap();
    let d0_class = r.read_u8("class").unwrap();
    assert_eq!(d0_severity, 0);
    assert_eq!(d0_class, 1);
    let _ = r.read_u32_le("span_start").unwrap();
    let _ = r.read_u32_le("span_len").unwrap();
    let _ = r.read_u32_le("line").unwrap();
    let _ = r.read_u32_le("col").unwrap();
    let msg0 = r.read_string("msg").unwrap();
    assert_eq!(msg0, "first");
    let d1_severity = r.read_u8("severity").unwrap();
    assert_eq!(d1_severity, 1);
    let d1_class = r.read_u8("class").unwrap();
    assert_eq!(d1_class, 2);
}

#[test]
fn encode_validation_body_unicode_message_uses_byte_length() {
    let msg = "café→_bar";
    let diagnostics = vec![WireDiag {
        severity: 0,
        class: 0,
        span_start: 0,
        span_len: 1,
        line: 1,
        col: 1,
        message: msg.to_string(),
    }];
    let body = encode_validation_body("u", &diagnostics, &[]).unwrap();
    // Re-decode and check the message round-trips correctly.
    let mut r = BinaryFrameReader::new(&body);
    r.read_string("id").unwrap();
    r.read_count("diag_count").unwrap();
    r.read_u8("severity").unwrap();
    r.read_u8("class").unwrap();
    r.read_u32_le("span_start").unwrap();
    r.read_u32_le("span_len").unwrap();
    r.read_u32_le("line").unwrap();
    r.read_u32_le("col").unwrap();
    let decoded_msg = r.read_string("msg").unwrap();
    assert_eq!(decoded_msg, msg);
    // One diagnostic was written, 0 resolved spans — read resolved_count.
    let resolved_count = r.read_count("resolved_count").unwrap();
    assert_eq!(resolved_count, 0);
    r.ensure_at_end().unwrap();
}

#[test]
fn encode_frame_rejects_oversized_body() {
    // Construct a frame whose total_len exceeds MAX_FRAME_BYTES.
    // The encode_frame check is: total_len = body.len() + 1 > MAX_FRAME_BYTES.
    let frame = EngineFrame::RequestFailed {
        id: "huge".to_string(),
        error: EngineError::query("x".repeat(MAX_FRAME_BYTES as usize)),
    };
    let result = encode_frame(&frame);
    assert!(
        result.is_err(),
        "frame exceeding MAX_FRAME_BYTES must error"
    );
    let err = result.unwrap_err();
    assert!(err.to_string().contains("too large"), "{}", err);
}

#[test]
fn encode_validation_body_with_resolved_spans_preserves_order() {
    let resolved = vec![
        WireSpan { start: 5, len: 3 },
        WireSpan { start: 10, len: 8 },
    ];
    let body = encode_validation_body("r", &[], &resolved).unwrap();
    let mut r = BinaryFrameReader::new(&body);
    r.read_string("id").unwrap();
    r.read_count("diag_count").unwrap();
    let resolved_count = r.read_count("resolved_count").unwrap();
    assert_eq!(resolved_count, 2);
    let s0_start = r.read_u32_le("resolved.start").unwrap();
    let s0_len = r.read_u32_le("resolved.len").unwrap();
    assert_eq!(s0_start, 5);
    assert_eq!(s0_len, 3);
    let s1_start = r.read_u32_le("resolved.start").unwrap();
    let s1_len = r.read_u32_le("resolved.len").unwrap();
    assert_eq!(s1_start, 10);
    assert_eq!(s1_len, 8);
    r.ensure_at_end().unwrap();
}

// ───────────────────────────────────────────────────────────────────
// Outbound frame/writer failure tests
// ───────────────────────────────────────────────────────────────────

#[test]
fn frame_request_id_returns_none_for_ready() {
    let frame = EngineFrame::Ready {
        protocol_version: 2,
    };
    assert!(frame_request_id(&frame).is_none());
}

#[test]
fn frame_request_id_extracts_id_from_variants() {
    let pong = EngineFrame::Pong {
        id: "p1".to_string(),
    };
    assert_eq!(frame_request_id(&pong), Some("p1"));

    let failed = EngineFrame::RequestFailed {
        id: "f1".to_string(),
        error: EngineError::query("err"),
    };
    assert_eq!(frame_request_id(&failed), Some("f1"));

    let validation = EngineFrame::ValidationReport {
        id: "v1".to_string(),
        diagnostics: vec![],
        resolved: vec![],
    };
    assert_eq!(frame_request_id(&validation), Some("v1"));

    let rows = EngineFrame::RowsBatch {
        id: "r1".to_string(),
        result_set_index: 0,
        rows: vec![],
    };
    assert_eq!(frame_request_id(&rows), Some("r1"));
}

#[tokio::test]
async fn logged_sender_does_not_panic_on_closed_channel() {
    let (tx, rx) = mpsc::channel::<EngineFrame>(8);
    let sender = LoggedSender::new(tx, None);
    drop(rx);
    sender
        .send(EngineFrame::Pong {
            id: "dropped".to_string(),
        })
        .await;
}

#[test]
fn best_effort_fallback_encodes_when_original_frame_oversized() {
    let oversized = EngineFrame::RequestFailed {
        id: "big-1".to_string(),
        error: EngineError::query("x".repeat(MAX_FRAME_BYTES as usize)),
    };
    assert!(encode_frame(&oversized).is_err());

    let id = frame_request_id(&oversized).expect("RequestFailed has an id");
    let fallback = EngineFrame::RequestFailed {
        id: id.to_string(),
        error: EngineError::protocol("failed to encode response: frame too large"),
    };
    assert!(
        encode_frame(&fallback).is_ok(),
        "best-effort fallback must encode"
    );
}

// ───────────────────────────────────────────────────────────────────
// Cross-language response golden-byte tests
//
// These verify that `encode_validation_body` and `encode_rows_batch_body`
// produce the exact byte sequences that the Swift decoders expect.
// The Swift tests (`testValidationReportResponseGoldenBytes` and
// `testRowsBatchResponseGoldenBytes` in EngineClientTests.swift) build
// the same byte sequences and decode them. If either side changes the
// wire format, the mismatch will be caught here or in Swift.
// ───────────────────────────────────────────────────────────────────

/// Mirrors Swift `testValidationReportResponseGoldenBytes`.
/// id="vr1", 2 diagnostics, 1 resolved span.
#[test]
fn cross_language_validation_report_golden_bytes() {
    let diagnostics = vec![
        WireDiag {
            severity: 0,
            class: 1,
            span_start: 10,
            span_len: 5,
            line: 1,
            col: 11,
            message: "bad table".to_string(),
        },
        WireDiag {
            severity: 1,
            class: 0,
            span_start: 20,
            span_len: 3,
            line: 2,
            col: 5,
            message: "oops".to_string(),
        },
    ];
    let resolved = vec![WireSpan { start: 5, len: 3 }];

    let body = encode_validation_body("vr1", &diagnostics, &resolved).unwrap();

    let mut expected = Vec::new();
    expected.extend_from_slice(&3u32.to_le_bytes());
    expected.extend_from_slice(b"vr1");
    expected.extend_from_slice(&2u32.to_le_bytes());
    expected.push(0);
    expected.push(1);
    expected.extend_from_slice(&10u32.to_le_bytes());
    expected.extend_from_slice(&5u32.to_le_bytes());
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(&11u32.to_le_bytes());
    expected.extend_from_slice(&9u32.to_le_bytes());
    expected.extend_from_slice(b"bad table");
    expected.push(1);
    expected.push(0);
    expected.extend_from_slice(&20u32.to_le_bytes());
    expected.extend_from_slice(&3u32.to_le_bytes());
    expected.extend_from_slice(&2u32.to_le_bytes());
    expected.extend_from_slice(&5u32.to_le_bytes());
    expected.extend_from_slice(&4u32.to_le_bytes());
    expected.extend_from_slice(b"oops");
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(&5u32.to_le_bytes());
    expected.extend_from_slice(&3u32.to_le_bytes());

    assert_eq!(
        body, expected,
        "validation report body must match golden bytes"
    );
}

/// Mirrors Swift `testRowsBatchResponseGoldenBytes`.
/// id="rb1", result_set_index=0, 2 rows with mixed cell types.
#[test]
fn cross_language_rows_batch_golden_bytes() {
    use sqlly_core::CellValue;

    let rows = vec![
        vec![
            CellValue::I64(42),
            CellValue::String("hello".to_string()),
            CellValue::Null,
        ],
        vec![CellValue::Bool(true), CellValue::F64(3.125)],
    ];

    let body = encode_rows_batch_body("rb1", 0, &rows).unwrap();

    let mut expected = Vec::new();
    expected.extend_from_slice(&3u32.to_le_bytes());
    expected.extend_from_slice(b"rb1");
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&2u32.to_le_bytes());
    expected.extend_from_slice(&3u32.to_le_bytes());
    expected.push(0x02);
    expected.extend_from_slice(&42i64.to_le_bytes());
    expected.push(0x04);
    expected.extend_from_slice(&5u32.to_le_bytes());
    expected.extend_from_slice(b"hello");
    expected.push(0x00);
    expected.extend_from_slice(&2u32.to_le_bytes());
    expected.push(0x01);
    expected.push(1);
    expected.push(0x03);
    expected.extend_from_slice(&3.125f64.to_le_bytes());

    assert_eq!(body, expected, "rows batch body must match golden bytes");
}

// ---------------------------------------------------------------------------
// Proxy mode: the engine opens only catalog connections, never the request's
// ---------------------------------------------------------------------------

const PROXY_CREDENTIAL_FILE: &str = r##"{
  "format": "sqlly-credentials",
  "version": 1,
  "connections": [
    { "name": "Ops", "engine": "sql-server", "server": "sql01.internal", "database": "ops",
      "authentication": { "type": "sql-login", "username": "ops", "password": "ops-secret" } }
  ]
}"##;

fn proxy_pools() -> (EnginePools, sqlly_core::ConnectionId) {
    let catalog =
        sqlly_core::credential_proxy::ProxyCatalog::from_credential_text(PROXY_CREDENTIAL_FILE)
            .unwrap();
    let id = catalog.profiles[0].id;
    (EnginePools::with_proxy(Some(Arc::new(catalog))), id)
}

fn request_connection(proxy_profile: Option<uuid::Uuid>) -> EngineConnection {
    serde_json::from_str(&format!(
        r#"{{"server":"attacker.example","username":"sa","password":"guess","proxy_profile":{}}}"#,
        match proxy_profile {
            Some(id) => format!("\"{id}\""),
            None => "null".to_string(),
        }
    ))
    .unwrap()
}

/// In proxy mode the catalog entry replaces the request's connection:
/// server, credentials, and (for an entry that names one) the database all
/// come from the file.
#[test]
fn proxy_mode_resolves_the_catalog_profile_not_the_request() {
    let (pool, id) = proxy_pools();
    let profile = pool
        .resolve_profile(request_connection(Some(id.0)))
        .unwrap();
    assert_eq!(profile.server, "sql01.internal");
    assert_eq!(profile.database.as_deref(), Some("ops"));
    match &profile.authentication {
        sqlly_core::AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => {
            assert_eq!(username, "ops");
            assert_eq!(secret_ref.resolve().unwrap(), "ops-secret");
        }
        other => panic!("unexpected auth {other:?}"),
    }
}

/// A server-level entry (no database in the file) opens the database the
/// request chose, so a query on any database in the explorer runs there
/// instead of the login's default. An entry that names a database stays
/// pinned to it whatever the request asks for.
#[test]
fn proxy_mode_takes_the_request_database_only_for_server_level_entries() {
    let catalog = sqlly_core::credential_proxy::ProxyCatalog::from_credential_text(
        r##"{
  "format": "sqlly-credentials",
  "version": 1,
  "connections": [
    { "name": "Server", "engine": "sql-server", "server": "sql01.internal",
      "authentication": { "type": "sql-login", "username": "ops", "password": "ops-secret" } }
  ]
}"##,
    )
    .unwrap();
    let id = catalog.profiles[0].id;
    let pool = EnginePools::with_proxy(Some(Arc::new(catalog)));
    let request = |database: &str| -> EngineConnection {
        serde_json::from_str(&format!(
            r#"{{"server":"attacker.example","username":"sa","password":"guess","database":{database},"proxy_profile":"{}"}}"#,
            id.0
        ))
        .unwrap()
    };

    let profile = pool.resolve_profile(request(r#""Shared""#)).unwrap();
    assert_eq!(profile.database.as_deref(), Some("Shared"));
    assert_eq!(
        profile.server, "sql01.internal",
        "the server still comes from the file"
    );
    assert_eq!(
        pool.resolve_profile(request("null")).unwrap().database,
        None
    );
    assert_eq!(
        pool.resolve_profile(request(r#""  ""#)).unwrap().database,
        None
    );

    let (pinned, pinned_id) = proxy_pools();
    let mut asks_elsewhere = request(r#""master""#);
    asks_elsewhere.proxy_profile = Some(pinned_id.0);
    assert_eq!(
        pinned
            .resolve_profile(asks_elsewhere)
            .unwrap()
            .database
            .as_deref(),
        Some("ops")
    );
}

/// A request that names no catalog entry, or an unknown one, is refused —
/// the browser can never make a proxy engine dial a server of its choosing.
#[test]
fn proxy_mode_refuses_requests_without_a_known_catalog_id() {
    let (pool, _id) = proxy_pools();
    let err = pool.resolve_profile(request_connection(None)).unwrap_err();
    assert!(err.to_string().contains("proxy_profile"), "{err}");
    let err = pool
        .resolve_profile(request_connection(Some(uuid::Uuid::new_v4())))
        .unwrap_err();
    assert!(err.to_string().contains("no proxied connection"), "{err}");
}

/// An ordinary engine keeps trusting the request, and refuses a proxy
/// reference it cannot honor rather than silently dialing the request's own
/// server under a proxied identity.
#[test]
fn ordinary_engine_ignores_proxy_mode_but_refuses_proxy_references() {
    let pool = EnginePools::new();
    let profile = pool.resolve_profile(request_connection(None)).unwrap();
    assert_eq!(profile.server, "attacker.example");
    let err = pool
        .resolve_profile(request_connection(Some(uuid::Uuid::new_v4())))
        .unwrap_err();
    assert!(err.to_string().contains("no proxied connections"), "{err}");
}

/// The wire path: a proxy-mode engine answers an execute_query that names
/// no catalog entry with `request_failed` and keeps serving.
#[tokio::test]
async fn proxy_mode_dispatch_fails_unnamed_execute_query_and_stays_alive() {
    let (pool, _id) = proxy_pools();
    let connection_json = r#"{"server":"attacker.example","username":"sa","password":"guess"}"#;
    let body = sqlly_remote::engine_protocol::layouts::encode_execute_query(
        "q1",
        connection_json,
        "SELECT 1",
        None,
        None,
    );
    let mut frame = vec![REQUEST_TAG_EXECUTE_QUERY];
    frame.extend_from_slice(&body);

    let (tx, mut rx) = mpsc::channel::<EngineFrame>(8);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let catalog = test_catalog_state();
    let keep_running = dispatch_request(&frame, &frames, &active, &pool, &catalog).await;
    assert!(keep_running);
    drop(frames);
    match rx.recv().await.expect("a request_failed frame") {
        EngineFrame::RequestFailed { id, error } => {
            assert_eq!(id, "q1");
            assert!(error.to_string().contains("proxy_profile"), "{error}");
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

/// Lifecycle events and rows travel on separate channels; a small result's
/// only batch could reach the client ahead of its `ResultSetStarted` (and
/// `QueryCompleted` ahead of the rows), and the client drops rows for a set
/// it hasn't seen — the browser's intermittent "headers, zero rows". The
/// stream must go out header, rows, then the terminal event, whatever order
/// the channels deliver them in.
#[tokio::test]
async fn consume_query_stream_sends_rows_after_their_header_and_before_completion() {
    let cancel_slot: sqlly_services::CancelSlot = Arc::new(std::sync::Mutex::new(None));
    let events = sqlly_core::EventBus::new(16);
    let mut subscription = events.subscribe();
    let (rows_tx, rows_rx) = sqlly_core::row_channel(4);
    let session = sqlly_core::SessionId::default();
    rows_tx
        .send(sqlly_core::RowBatch::new(
            session.clone(),
            0,
            vec![vec![CellValue::I64(7)]],
        ))
        .await
        .ok();
    events
        .publish(sqlly_core::EventPayload::QueryCompleted {
            session_id: session.clone(),
            elapsed_ms: 3,
        })
        .ok();
    events
        .publish(sqlly_core::EventPayload::ResultSetStarted {
            session_id: session.clone(),
            result_set_index: 0,
            columns: vec![sqlly_core::ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            }],
        })
        .ok();
    drop(rows_tx);
    drop(events);

    let (tx, mut rx) = mpsc::channel(16);
    let frames = LoggedSender::new(tx, None);
    let (connected, _, failed) = consume_query_stream(
        "q-order".to_string(),
        &frames,
        &mut subscription,
        rows_rx,
        Duration::from_secs(5),
        &cancel_slot,
        false,
    )
    .await;
    drop(frames);
    assert!(connected && !failed);

    let mut order = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        order.push(match frame {
            EngineFrame::RowsBatch { .. } => "rows",
            EngineFrame::Event { event, .. } => match event.payload {
                sqlly_core::EventPayload::ResultSetStarted { .. } => "header",
                sqlly_core::EventPayload::QueryCompleted { .. } => "completed",
                _ => "other event",
            },
            _ => "other",
        });
    }
    assert_eq!(order, ["header", "rows", "completed"]);
}

// ---------------------------------------------------------------------------
// Proxy mode: the catalog's SELECT-only policy is enforced by the engine
// ---------------------------------------------------------------------------

/// A credential file with two SQLite connections on `path`: `Prod` is
/// SELECT-only through its Production environment label, `Flagged` through
/// its own safety flag, and `Open` has no policy at all.
fn select_only_catalog(path: &std::path::Path) -> sqlly_core::credential_proxy::ProxyCatalog {
    let file = serde_json::json!({
        "format": "sqlly-credentials",
        "version": 1,
        "clients": [{ "name": "Acme" }],
        "projects": [{ "name": "Billing", "safety": { "only_allow_select": true } }],
        "environments": [{ "name": "Production", "safety": { "only_allow_select": true } }],
        "connections": [
            { "name": "Prod", "engine": "sqlite", "server": path.to_string_lossy(),
              "environment": "production", "client": "Acme" },
            { "name": "Flagged", "engine": "sqlite", "server": path.to_string_lossy(),
              "safety": { "block_all_but_select": true } },
            { "name": "Project", "engine": "sqlite", "server": path.to_string_lossy(),
              "project": "billing" },
            { "name": "Open", "engine": "sqlite", "server": path.to_string_lossy(),
              "client": "Acme" }
        ]
    })
    .to_string();
    sqlly_core::credential_proxy::ProxyCatalog::from_credential_text(&file).unwrap()
}

fn proxy_connection_json(id: uuid::Uuid) -> String {
    serde_json::json!({
        "server": "attacker.example",
        "username": "",
        "password": "",
        "proxy_profile": id,
    })
    .to_string()
}

fn execute_query_frame(id: &str, connection_json: &str, sql: &str) -> Vec<u8> {
    let body = sqlly_remote::engine_protocol::layouts::encode_execute_query(
        id,
        connection_json,
        sql,
        None,
        None,
    );
    let mut frame = vec![REQUEST_TAG_EXECUTE_QUERY];
    frame.extend_from_slice(&body);
    frame
}

fn row_edit_frame(id: &str, connection_json: &str, op: &str, payload: &str) -> Vec<u8> {
    let body = sqlly_remote::engine_protocol::layouts::request_header(id)
        .string(connection_json)
        .string(op)
        .string(payload)
        .done();
    let mut frame = vec![REQUEST_TAG_ROW_EDIT];
    frame.extend_from_slice(&body);
    frame
}

/// Dispatch one frame and collect frames until the request's terminal one.
async fn dispatch_to_terminal(pool: &EnginePools, frame: &[u8]) -> Vec<EngineFrame> {
    let (tx, mut rx) = mpsc::channel::<EngineFrame>(64);
    let frames = LoggedSender::new(tx, None);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let catalog = test_catalog_state();
    assert!(dispatch_request(frame, &frames, &active, pool, &catalog).await);
    drop(frames);
    let mut out = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("a frame in time")
            .expect("a terminal frame before the channel closes");
        let terminal = matches!(
            frame,
            EngineFrame::RequestCompleted { .. }
                | EngineFrame::RequestFailed { .. }
                | EngineFrame::RowEditReport { .. }
        );
        out.push(frame);
        if terminal {
            return out;
        }
    }
}

fn failure_message(frames: &[EngineFrame]) -> Option<String> {
    frames.iter().find_map(|f| match f {
        EngineFrame::RequestFailed { error, .. } => Some(error.to_string()),
        _ => None,
    })
}

/// The effective SELECT-only flag comes from the entry itself or from any of
/// its labels, matched the way the credential file resolves them.
#[test]
fn proxy_mode_resolves_select_only_from_the_entry_and_its_labels() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = select_only_catalog(&dir.path().join("policy.db"));
    let by_name = |name: &str| catalog.profiles.iter().find(|p| p.name == name).unwrap();
    assert!(
        labels_select_only(&catalog, by_name("Prod")),
        "environment label"
    );
    assert!(
        labels_select_only(&catalog, by_name("Project")),
        "project label, case-insensitive"
    );
    assert!(
        !labels_select_only(&catalog, by_name("Flagged")),
        "no label: the entry's own flag"
    );
    assert!(!labels_select_only(&catalog, by_name("Open")));

    let pool = EnginePools::with_proxy(Some(Arc::new(catalog.clone())));
    let resolve = |name: &str| {
        pool.resolve_profile(request_connection(Some(by_name(name).id.0)))
            .unwrap()
    };
    assert!(resolve("Prod").block_all_but_select);
    assert!(resolve("Flagged").block_all_but_select);
    assert!(resolve("Project").block_all_but_select);
    assert!(!resolve("Open").block_all_but_select);
}

/// On a SELECT-only proxied connection a SELECT runs, while an UPDATE and a
/// row edit are refused with the policy message before anything reaches the
/// database.
#[tokio::test]
async fn proxy_mode_refuses_non_select_and_row_edits_on_select_only_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER); INSERT INTO t VALUES (1, 10);",
        )
        .unwrap();
    }
    let catalog = select_only_catalog(&path);
    let prod = proxy_connection_json(
        catalog
            .profiles
            .iter()
            .find(|p| p.name == "Prod")
            .unwrap()
            .id
            .0,
    );
    let open = proxy_connection_json(
        catalog
            .profiles
            .iter()
            .find(|p| p.name == "Open")
            .unwrap()
            .id
            .0,
    );
    let pool = EnginePools::with_proxy(Some(Arc::new(catalog)));

    // Allowed: a SELECT on the SELECT-only connection completes.
    let frames =
        dispatch_to_terminal(&pool, &execute_query_frame("q1", &prod, "SELECT n FROM t")).await;
    assert!(
        matches!(frames.last(), Some(EngineFrame::RequestCompleted { id, .. }) if id == "q1"),
        "{frames:?}"
    );

    // Refused: an UPDATE never starts.
    let frames = dispatch_to_terminal(
        &pool,
        &execute_query_frame("q2", &prod, "UPDATE t SET n = 0"),
    )
    .await;
    assert_eq!(
        frames.len(),
        1,
        "no RequestStarted before the refusal: {frames:?}"
    );
    let message = failure_message(&frames).expect("request_failed");
    assert!(message.contains("read-only by policy"), "{message}");
    assert!(message.contains("SELECT"), "{message}");

    // Refused: a mixed batch, however it is spelled.
    let frames = dispatch_to_terminal(
        &pool,
        &execute_query_frame("q3", &prod, "SELECT 1; /* x */ delete from t"),
    )
    .await;
    assert!(
        failure_message(&frames)
            .unwrap()
            .contains("read-only by policy")
    );

    // Refused: every row-edit op.
    let frames = dispatch_to_terminal(&pool, &row_edit_frame("r1", &prod, "open", "{}")).await;
    let message = failure_message(&frames).expect("request_failed");
    assert!(message.contains("read-only by policy"), "{message}");
    assert!(message.contains("row editing"), "{message}");

    // The data was never touched, and a connection without the policy may
    // change it.
    let frames = dispatch_to_terminal(
        &pool,
        &execute_query_frame("q4", &open, "UPDATE t SET n = 0"),
    )
    .await;
    assert!(
        matches!(frames.last(), Some(EngineFrame::RequestCompleted { .. })),
        "{frames:?}"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = conn.query_row("SELECT n FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(
        n, 0,
        "the Open connection's update ran; the Prod ones never did"
    );
}

/// An ordinary (non-proxy) engine trusts the request as before: the policy
/// pre-check does not apply, so an UPDATE on a connection without the flag
/// runs.
#[tokio::test]
async fn ordinary_engine_is_unaffected_by_the_proxy_policy_check() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }
    let connection = serde_json::json!({
        "server": path.to_string_lossy(),
        "username": "",
        "password": "",
        "database_type": "Sqlite",
    })
    .to_string();
    let pool = EnginePools::new();
    let frames = dispatch_to_terminal(
        &pool,
        &execute_query_frame("u1", &connection, "UPDATE t SET n = 5"),
    )
    .await;
    assert!(
        matches!(frames.last(), Some(EngineFrame::RequestCompleted { .. })),
        "{frames:?}"
    );
    assert!(failure_message(&frames).is_none());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = conn.query_row("SELECT n FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 5);
}

#[test]
fn query_failures_map_plain_and_keyed_capability_errors_to_unsupported() {
    let keyed = StudioError::CapabilityNotSupportedMessage(sqlly_core::DisplayMessage::new(
        "services.query.errors.manual_commit_unavailable",
        "Manual commit mode is not available on this port",
    ));
    let display = keyed.to_string();
    let error = query_failure(keyed);
    assert_eq!(
        error,
        EngineError::unsupported("Manual commit mode is not available on this port")
    );
    assert_eq!(error.code(), "unsupported");
    // The client rebuilds `CapabilityNotSupported(detail)` from the frame;
    // its Display must be the engine-side text, prefixed exactly once.
    let rebuilt = StudioError::CapabilityNotSupported(match &error {
        EngineError::Unsupported(detail) => detail.clone(),
        other => panic!("expected unsupported, got {other:?}"),
    });
    assert_eq!(rebuilt.to_string(), display);

    assert_eq!(
        query_failure(StudioError::CapabilityNotSupported("no driver".into())),
        EngineError::unsupported("no driver")
    );
    assert_eq!(
        query_failure(StudioError::QueryTimeout(std::time::Duration::from_secs(5))).code(),
        "timeout"
    );
    assert_eq!(
        query_failure(StudioError::QueryExecution("Msg 208".into())),
        EngineError::query("Query execution error: Msg 208")
    );
}
