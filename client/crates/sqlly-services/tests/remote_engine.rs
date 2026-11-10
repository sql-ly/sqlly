//! Drives `RemoteServices` against a REAL `sqlly-cli engine` subprocess over
//! stdio (mirroring `apps/cli/tests/engine_io.rs` and
//! the bridge's end-to-end tests). This proves the request encoders and
//! response decoders match the engine byte-for-byte:
//!
//!   * a `ping`/health round trip,
//!   * `execute_query` against an unreachable server terminates with the
//!     engine's `request_failed` decoded into a `QueryEvent::Error`,
//!   * `sync_catalog` and `validate_sql` round-trip into the native catalog
//!     types.
//!
//! `sqlly-cli` lives in another crate, so `CARGO_BIN_EXE_*` is unavailable; the
//! binary is located next to this test (building it on demand), exactly as the
//! bridge end-to-end test does.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use sqlly_ai::catalog::{
    CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
};
use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_remote::engine_protocol::AsyncFramedTransport;
use sqlly_services::QueryEvent;
use sqlly_services::remote::RemoteServices;
use tokio::io::Join;
use tokio::process::{ChildStdin, ChildStdout, Command};

type EngineTransport = AsyncFramedTransport<Join<ChildStdout, ChildStdin>>;

/// A spawned engine plus the remote-service bundle wired to its stdio. Holds the
/// `Child` so the process outlives the transport; dropping the bundle closes the
/// engine's stdin and it exits cleanly.
struct Harness {
    _child: tokio::process::Child,
    remote: RemoteServices<EngineTransport>,
}

fn locate_sqlly_cli() -> PathBuf {
    if let Ok(p) = std::env::var("SQLLY_CLI_BIN")
        && !p.trim().is_empty()
    {
        return PathBuf::from(p);
    }
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // drop test binary name -> .../deps
    if dir.ends_with("deps") {
        dir.pop(); // -> .../<profile>
    }
    let candidate = dir.join(format!("sqlly-cli{}", std::env::consts::EXE_SUFFIX));
    if candidate.exists() {
        return candidate;
    }
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    let status = std::process::Command::new(cargo)
        .args(["build", "-p", "sqlly-cli"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("run `cargo build -p sqlly-cli`");
    assert!(status.success(), "`cargo build -p sqlly-cli` failed");
    assert!(
        candidate.exists(),
        "sqlly-cli binary missing at {} even after building",
        candidate.display()
    );
    candidate
}

fn spawn_harness(catalog_dir: &std::path::Path) -> Harness {
    let bin = locate_sqlly_cli();
    let mut child = Command::new(bin)
        .arg("engine")
        .env("SQLLY_CATALOG_DIR", catalog_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sqlly-cli engine");
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let transport = AsyncFramedTransport::new(tokio::io::join(stdout, stdin));
    let remote = RemoteServices::from_transport(transport);
    Harness {
        _child: child,
        remote,
    }
}

fn unique_catalog_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sqlly-remote-svc-{label}-{nanos}"))
}

/// A local SQL-login profile whose catalog operations never open a socket.
fn catalog_profile() -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(
        "remote-test",
        "test.local",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "p".to_string(),
        },
    );
    profile.database = Some("appdb".to_string());
    profile
}

fn sample_objects() -> Vec<CatalogObject> {
    let table = |name: &str, columns: Vec<CatalogColumn>| CatalogObject {
        schema: Some("dbo".to_string()),
        name: name.to_string(),
        kind: CatalogObjectKind::Table,
        columns,
        primary_key: Vec::new(),
        foreign_keys: Vec::new(),
        parameters: Vec::new(),
        definition: None,
        source: CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: Some(1_700_000_000_000),
            modify_date: Some(1_700_000_000_000),
        },
    };
    let col = |name: &str, ordinal: i64| CatalogColumn {
        name: name.to_string(),
        type_name: Some("int".to_string()),
        ordinal,
        nullable: false,
    };
    vec![
        table("Orders", vec![col("OrderId", 0), col("CustomerId", 1)]),
        table("Customers", vec![col("CustomerId", 0)]),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_round_trip() {
    let dir = unique_catalog_dir("ping");
    let harness = spawn_harness(&dir);
    harness
        .remote
        .ping()
        .await
        .expect("engine ping must answer with pong");
    let _ = std::fs::remove_dir_all(&dir);
}

/// New client ↔ new engine: the real engine's `Ready` advertises the row-edit
/// capabilities a relay client gates on, and the remote row-editing service
/// reports them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_engine_advertises_row_edit_capabilities() {
    let dir = unique_catalog_dir("caps");
    let harness = spawn_harness(&dir);
    let caps = harness
        .remote
        .engine()
        .wait_for_capabilities(Duration::from_secs(30))
        .await
        .expect("the engine announces itself with Ready");
    assert!(caps.supports(sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS));
    assert!(caps.supports(sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_FK_LABELS));
    assert!(caps.supports(sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_APPLY_BATCH));
    assert!(
        harness
            .remote
            .services()
            .row_editing
            .server_expressions_supported()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_query_against_unreachable_server_terminates_with_error() {
    let dir = unique_catalog_dir("unreachable");
    let harness = spawn_harness(&dir);

    // RFC 5737 documentation address: never routable. A short timeout keeps the
    // engine's connect/watchdog bounded.
    let mut profile = ConnectionProfile::sql_login(
        "unreachable",
        "192.0.2.1",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "x".to_string(),
        },
    );
    profile.port = 1433;
    profile.database = Some("x".to_string());
    profile.encrypt = true;
    profile.trust_server_certificate = true;
    profile.timeout_seconds = 2;

    let handle = harness
        .remote
        .services()
        .queries
        .execute(&profile, "SELECT 1", 32)
        .await
        .expect("execute must accept the request");

    let mut events = handle.events;
    let mut error_message: Option<String> = None;
    let mut saw_terminal = false;
    let drain = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = events.recv().await {
            match event {
                QueryEvent::Error { message } => {
                    error_message = Some(message);
                    saw_terminal = true;
                }
                QueryEvent::Completed { .. } | QueryEvent::Cancelled => {
                    saw_terminal = true;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(drain.is_ok(), "the query stream must terminate, never hang");
    assert!(saw_terminal, "a terminal QueryEvent must arrive");
    let message = error_message.expect("unreachable server must yield a decoded Error event");
    assert!(
        !message.trim().is_empty(),
        "the engine's request_failed error must be decoded into the message: {message:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_catalog_then_validate_sql_round_trip() {
    let dir = unique_catalog_dir("catalog");
    let harness = spawn_harness(&dir);
    let profile = catalog_profile();
    let catalog = &harness.remote.services().catalog;

    // sync_catalog: two brand-new objects into an empty store => added == 2.
    let report = catalog
        .sync_catalog(
            &profile,
            sample_objects(),
            sqlly_ai::catalog::SyncCoverage::FullSource,
        )
        .await
        .expect("sync_catalog must round-trip a catalog_sync_report");
    assert_eq!(report.added, 2, "both new objects must be reported added");
    assert_eq!(report.removed, 0);

    // validate_sql: a trivial statement validates against the seeded catalog and
    // returns a typed SqlValidationResult (no request_failed).
    let validation = catalog
        .validate_sql(&profile, "SELECT 1", Dialect::SqlServer)
        .await
        .expect("validate_sql must round-trip a sql_validation_report");
    // The result decoded into the native type; a trivial constant SELECT
    // references no catalog objects, so validation succeeds.
    assert!(
        validation.is_valid,
        "SELECT 1 must validate cleanly: {:?}",
        validation.issues
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Parity smoke: cancelling a remote query terminates the stream with a
/// terminal event, never a hang — the cancel frame round-trips the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_terminates_the_remote_stream() {
    let dir = unique_catalog_dir("cancel-parity");
    let harness = spawn_harness(&dir);

    // Unreachable server with a generous timeout: without the cancel this
    // stream would stay pending for the full connect timeout.
    let mut profile = ConnectionProfile::sql_login(
        "cancel-target",
        "192.0.2.1",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "x".to_string(),
        },
    );
    profile.port = 1433;
    profile.database = Some("x".to_string());
    profile.encrypt = true;
    profile.trust_server_certificate = true;
    profile.timeout_seconds = 30;

    let queries = &harness.remote.services().queries;
    let handle = queries
        .execute(&profile, "SELECT 1", 32)
        .await
        .expect("execute must accept the request");
    let task_id = handle.task_id.clone();

    tokio::time::sleep(Duration::from_millis(200)).await;
    queries.cancel(&task_id).await.expect("cancel must send");

    let mut events = handle.events;
    let mut terminals = Vec::new();
    let drain = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if matches!(
                event,
                QueryEvent::Cancelled | QueryEvent::Error { .. } | QueryEvent::Completed { .. }
            ) {
                terminals.push(event);
            }
        }
    })
    .await;
    assert!(
        drain.is_ok(),
        "the cancelled stream must terminate well before the 30s connect timeout"
    );
    // The host blackholes and the connect timeout is 30s, so the only way the
    // stream can end this early is the cancel round-tripping the engine — the
    // terminal must be Cancelled specifically, not a coincidental error, and
    // it must arrive exactly once.
    assert!(
        matches!(terminals.as_slice(), [QueryEvent::Cancelled]),
        "cancel must terminate the stream with exactly one Cancelled event, got {terminals:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Parity smoke: catalog search round-trips the wire — objects synced through
/// the engine come back from `search_schema` as decoded hits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_catalog_then_search_schema_round_trip() {
    let dir = unique_catalog_dir("search-parity");
    let harness = spawn_harness(&dir);
    let profile = catalog_profile();
    let catalog = &harness.remote.services().catalog;

    catalog
        .sync_catalog(
            &profile,
            sample_objects(),
            sqlly_ai::catalog::SyncCoverage::FullSource,
        )
        .await
        .expect("sync_catalog must round-trip");

    let hits = catalog
        .search_schema(&profile, "orders", Some(10), Vec::new())
        .await
        .expect("search_schema must round-trip decoded hits");
    assert!(
        hits.iter().any(|h| h.name.eq_ignore_ascii_case("orders")),
        "the synced Orders object must be findable remotely: {hits:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A SQLite profile the engine opens from `path`.
fn sqlite_profile(path: &std::path::Path) -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(
        "remote-sqlite",
        path.to_string_lossy().to_string(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = sqlly_core::DatabaseType::Sqlite;
    profile.port = 0;
    profile.encrypt = false;
    profile
}

/// Run `sql` through the remote engine and collect every row it returns.
async fn remote_rows(
    harness: &Harness,
    profile: &ConnectionProfile,
    sql: &str,
) -> Vec<Vec<sqlly_core::CellValue>> {
    let handle = harness
        .remote
        .services()
        .queries
        .execute(profile, sql, 64)
        .await
        .expect("execute");
    let mut events = handle.events;
    let mut rows = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = events.recv().await {
            match event {
                QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
                QueryEvent::Error { message } => panic!("{sql}: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("query terminates");
    rows
}

fn rename(id: i64, from: &str, to: &str) -> sqlly_services::row_editing::StagedOp {
    use sqlly_core::CellValue;
    sqlly_services::row_editing::StagedOp::Update(sqlly_services::row_editing::UpdateRowRequest {
        target: sqlly_services::row_editing::TableTarget {
            schema: "main".to_string(),
            table: "people".to_string(),
        },
        key: vec![("id".to_string(), CellValue::I64(id))],
        rowversion: None,
        changes: vec![("name".to_string(), CellValue::String(to.to_string()))],
        original_values: vec![("name".to_string(), CellValue::String(from.to_string()))],
        expressions: Vec::new(),
    })
}

/// New client ↔ new engine: a staged grid save travels as the `apply_batch`
/// row-edit op and runs in ONE transaction with the local save's guards — a
/// statement that hits the wrong number of rows rolls the whole batch back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_batch_commits_and_rolls_back_through_the_real_engine() {
    use sqlly_services::row_editing::{BatchOutcome, BatchRequest, StagedBatch};
    let dir = unique_catalog_dir("batch");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("people.db");
    let harness = spawn_harness(&dir);
    let profile = sqlite_profile(&db);
    remote_rows(
        &harness,
        &profile,
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT); \
         INSERT INTO people VALUES (1, 'ada'), (2, 'grace');",
    )
    .await;
    let row_editing = &harness.remote.services().row_editing;
    harness
        .remote
        .engine()
        .wait_for_capabilities(Duration::from_secs(30))
        .await
        .expect("ready");
    assert!(row_editing.batch_commit_supported());

    let batch = |ops| BatchRequest {
        batch: StagedBatch {
            target: Some(sqlly_services::row_editing::TableTarget {
                schema: "main".to_string(),
                table: "people".to_string(),
            }),
            key_columns: vec!["id".to_string()],
            ops,
        },
    };

    // The second update's original value is stale: 0 rows affected, so the
    // first update must be rolled back with it.
    let outcome = row_editing
        .apply_batch(
            &profile,
            batch(vec![rename(1, "ada", "Ada"), rename(2, "stale", "Grace")]),
        )
        .await
        .expect("batch runs");
    assert_eq!(
        outcome,
        BatchOutcome::Conflict {
            statement_index: 1,
            affected: 0
        }
    );
    let names = remote_rows(&harness, &profile, "SELECT name FROM people ORDER BY id").await;
    assert_eq!(
        names,
        vec![
            vec![sqlly_core::CellValue::String("ada".into())],
            vec![sqlly_core::CellValue::String("grace".into())]
        ],
        "a conflict rolls back the whole batch"
    );

    let outcome = row_editing
        .apply_batch(
            &profile,
            batch(vec![rename(1, "ada", "Ada"), rename(2, "grace", "Grace")]),
        )
        .await
        .expect("batch runs");
    assert_eq!(outcome, BatchOutcome::Committed { statements: 2 });
    let names = remote_rows(&harness, &profile, "SELECT name FROM people ORDER BY id").await;
    assert_eq!(
        names,
        vec![
            vec![sqlly_core::CellValue::String("Ada".into())],
            vec![sqlly_core::CellValue::String("Grace".into())]
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
