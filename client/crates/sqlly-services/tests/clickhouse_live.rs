//! Every ClickHouse-specific SQL generator in the service layer, run against
//! a real server: the catalog SQL behind the explorer and IntelliSense, the
//! disk-usage, dashboard and activity queries, the server-info probe, the
//! maintenance statement, and plan capture + parsing. A generator whose SQL
//! ClickHouse rejects fails here instead of in the app.
//!
//! Opt-in: set `SQLLY_CLICKHOUSE_URL` (for example
//! `http://default:sqlly@127.0.0.1:8123/default`, the docker container from
//! `docker run -d --name sqlly-clickhouse -p 8123:8123 -e
//! CLICKHOUSE_PASSWORD=sqlly clickhouse/clickhouse-server`); every test
//! self-skips without it.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType};
use sqlly_engines::clickhouse::{
    ClickHouseClient, execute_batch_on_client, live_profile_from_env, open_client,
    query_rows_on_client,
};
use sqlly_intel::metadata::MetadataQuery;
use sqlly_intel::metadata::dialect::sql_for_engine;
use sqlly_services::admin::activity::ActivityMonitor;
use sqlly_services::admin::disk_usage::{DiskDialect, DiskUsage};
use sqlly_services::maintenance::{MaintenanceOp, MaintenanceTarget, maintenance_sql};
use sqlly_services::plans::{
    PlanDialect, estimated_capture_sql, explain_capture_sql, parse_captured_plan,
};
use sqlly_services::{ActivityDialect, Dashboard, DashboardDialect};

const DB: &str = "sqlly_dialects";

/// The fixture database is rebuilt once per test process (the tests share
/// it read-only), so row counts stay exact across runs.
static FIXTURE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

fn schema_sql() -> String {
    format!(
        "DROP DATABASE IF EXISTS {DB};
CREATE DATABASE {DB};
CREATE TABLE IF NOT EXISTS {DB}.customers (id UInt32, name String, mood Enum8('sad' = 1, 'ok' = 2, 'happy' = 3), tags Array(String), created DateTime DEFAULT now(), name_upper String MATERIALIZED upper(name), CONSTRAINT name_not_blank CHECK name <> '', INDEX name_idx name TYPE bloom_filter GRANULARITY 1) ENGINE = MergeTree ORDER BY id COMMENT 'people';
CREATE TABLE IF NOT EXISTS {DB}.orders (id UInt32, customer_id UInt32, amount Decimal(12, 2), placed_at DateTime64(3, 'UTC')) ENGINE = MergeTree PARTITION BY toYYYYMM(placed_at) ORDER BY (customer_id, id);
CREATE VIEW IF NOT EXISTS {DB}.big_orders AS SELECT o.id, c.name, o.amount FROM {DB}.orders o JOIN {DB}.customers c ON c.id = o.customer_id WHERE o.amount > 100;
CREATE MATERIALIZED VIEW IF NOT EXISTS {DB}.order_totals ENGINE = SummingMergeTree ORDER BY customer_id AS SELECT customer_id, sum(amount) AS total FROM {DB}.orders GROUP BY customer_id;
CREATE FUNCTION IF NOT EXISTS sqlly_tax AS (amount, rate) -> amount * rate;
INSERT INTO {DB}.customers (id, name, mood, tags) VALUES (1, 'Ada', 'happy', ['vip']), (2, 'Grace', 'ok', []);
INSERT INTO {DB}.orders VALUES (1, 1, 150.00, '2024-01-02 10:00:00'), (2, 2, 20.50, '2024-01-03 11:00:00'), (3, 1, 999.99, '2024-02-01 12:00:00')"
    )
}

struct Fixture {
    profile: ConnectionProfile,
    client: ClickHouseClient,
}

async fn fixture() -> Option<Fixture> {
    let mut profile = live_profile_from_env("clickhouse-dialects")?;
    FIXTURE
        .get_or_init(|| async {
            let bootstrap = open_client(&profile).await.expect("connect");
            execute_batch_on_client(&bootstrap, &schema_sql())
                .await
                .expect("fixture schema");
        })
        .await;
    profile.database = Some(DB.to_string());
    let client = open_client(&profile).await.expect("connect to fixture db");
    Some(Fixture { profile, client })
}

async fn rows(client: &ClickHouseClient, sql: &str) -> Vec<Vec<CellValue>> {
    match query_rows_on_client(client, sql).await {
        Ok(set) => set.rows,
        Err(e) => panic!("ClickHouse rejected generated SQL:\n{sql}\n{e}"),
    }
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

fn names(rows: &[Vec<CellValue>], col: usize) -> Vec<String> {
    rows.iter().map(|r| text(&r[col])).collect()
}

/// Every variant of [`MetadataQuery`] with arguments that name the fixture.
fn every_query() -> Vec<MetadataQuery> {
    let s = || DB.to_string();
    vec![
        MetadataQuery::ServerCapability,
        MetadataQuery::ServerPermissions,
        MetadataQuery::ServerConfiguration,
        MetadataQuery::Databases,
        MetadataQuery::Schemas,
        MetadataQuery::SchemaObjectCounts,
        MetadataQuery::Tables { schema: None },
        MetadataQuery::Tables { schema: Some(s()) },
        MetadataQuery::Views { schema: None },
        MetadataQuery::Views { schema: Some(s()) },
        MetadataQuery::ProgrammableObjects { schema: None },
        MetadataQuery::ProgrammableObjects { schema: Some(s()) },
        MetadataQuery::ObjectInventory { schema: None },
        MetadataQuery::ObjectInventory { schema: Some(s()) },
        MetadataQuery::Columns {
            schema: s(),
            object: "customers".into(),
        },
        MetadataQuery::AllColumns,
        MetadataQuery::AllForeignKeys,
        MetadataQuery::AllPrimaryKeys,
        MetadataQuery::ProcedureParameters {
            schema: s(),
            object: "sqlly_tax".into(),
        },
        MetadataQuery::Indexes {
            schema: s(),
            table: "customers".into(),
        },
        MetadataQuery::Triggers {
            schema: s(),
            table: "orders".into(),
        },
        MetadataQuery::Statistics {
            schema: s(),
            table: "orders".into(),
        },
        MetadataQuery::Constraints {
            schema: s(),
            table: "customers".into(),
        },
        MetadataQuery::Types { schema: None },
        MetadataQuery::DatabaseTriggers { schema: None },
        MetadataQuery::Sequences { schema: None },
        MetadataQuery::Synonyms { schema: None },
        MetadataQuery::Extensions,
        MetadataQuery::AvailableExtensions,
        MetadataQuery::Events { schema: None },
        MetadataQuery::MaterializedViews { schema: None },
        MetadataQuery::MaterializedViews { schema: Some(s()) },
        MetadataQuery::TriggerDefinition {
            schema: s(),
            name: "nope".into(),
        },
        MetadataQuery::ConstraintDefinition {
            schema: s(),
            table: "customers".into(),
            name: "name_not_blank".into(),
        },
        MetadataQuery::ConstraintDefinition {
            schema: s(),
            table: "orders".into(),
            name: "PRIMARY".into(),
        },
        MetadataQuery::TypeDetail {
            schema: s(),
            type_name: "x".into(),
        },
        MetadataQuery::TableGeneral {
            schema: s(),
            table: "orders".into(),
        },
        MetadataQuery::DatabaseGeneral { database: s() },
        MetadataQuery::DatabaseFiles { database: s() },
        MetadataQuery::DatabasePermissions { database: s() },
        MetadataQuery::ColumnGeneral {
            schema: s(),
            table: "orders".into(),
            column: "amount".into(),
        },
        MetadataQuery::ObjectGeneral {
            schema: s(),
            object: "big_orders".into(),
        },
        MetadataQuery::ObjectGeneral {
            schema: s(),
            object: "sqlly_tax".into(),
        },
        MetadataQuery::IndexGeneral {
            schema: s(),
            table: "customers".into(),
            index: "name_idx".into(),
        },
        MetadataQuery::IndexGeneral {
            schema: s(),
            table: "orders".into(),
            index: "PRIMARY".into(),
        },
        MetadataQuery::IndexColumns {
            schema: s(),
            table: "orders".into(),
            index: "PRIMARY".into(),
        },
        MetadataQuery::ColumnDefaults {
            schema: s(),
            table: "customers".into(),
        },
        MetadataQuery::TableIndexColumns {
            schema: s(),
            table: "customers".into(),
        },
        MetadataQuery::DependentViews {
            schema: s(),
            table: "orders".into(),
        },
        MetadataQuery::StatisticGeneral {
            schema: s(),
            table: "orders".into(),
            statistic: "x".into(),
        },
        MetadataQuery::StatisticColumns {
            schema: s(),
            table: "orders".into(),
            statistic: "x".into(),
        },
        MetadataQuery::ObjectDefinition {
            schema: s(),
            object: "big_orders".into(),
        },
        MetadataQuery::ObjectDefinition {
            schema: s(),
            object: "sqlly_tax".into(),
        },
        MetadataQuery::ObjectPermissions {
            schema: s(),
            object: "orders".into(),
        },
    ]
}

#[tokio::test]
async fn every_metadata_query_runs_on_clickhouse() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    for query in every_query() {
        let sql = sql_for_engine(&query, DatabaseType::ClickHouse).unwrap();
        let _ = rows(&fx.client, &sql).await;
    }
}

async fn run(fx: &Fixture, query: MetadataQuery) -> Vec<Vec<CellValue>> {
    let sql = sql_for_engine(&query, DatabaseType::ClickHouse).unwrap();
    rows(&fx.client, &sql).await
}

#[tokio::test]
async fn catalog_queries_describe_the_fixture() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    let dbs = run(&fx, MetadataQuery::Databases).await;
    assert!(names(&dbs, 0).contains(&DB.to_string()));

    let schemas = run(&fx, MetadataQuery::Schemas).await;
    assert_eq!(names(&schemas, 0), vec![DB]);

    let counts = run(&fx, MetadataQuery::SchemaObjectCounts).await;
    // 2 tables + 1 view + 1 materialized view (+ its inner table).
    assert!(text(&counts[0][1]).parse::<i64>().unwrap() >= 4);

    let tables = run(
        &fx,
        MetadataQuery::Tables {
            schema: Some(DB.into()),
        },
    )
    .await;
    let table_names = names(&tables, 1);
    assert!(table_names.contains(&"customers".to_string()));
    assert!(table_names.contains(&"orders".to_string()));
    assert!(!table_names.contains(&"big_orders".to_string()));
    assert!(!table_names.contains(&"order_totals".to_string()));

    let views = run(&fx, MetadataQuery::Views { schema: None }).await;
    assert_eq!(names(&views, 1), vec!["big_orders"]);
    let materialized = run(&fx, MetadataQuery::MaterializedViews { schema: None }).await;
    assert_eq!(names(&materialized, 1), vec!["order_totals"]);

    let routines = run(&fx, MetadataQuery::ProgrammableObjects { schema: None }).await;
    let tax = routines
        .iter()
        .find(|r| text(&r[1]) == "sqlly_tax")
        .unwrap();
    assert_eq!(text(&tax[2]), "FN");
    assert_eq!(text(&tax[0]), DB);

    let inventory = run(&fx, MetadataQuery::ObjectInventory { schema: None }).await;
    let customers = inventory
        .iter()
        .find(|r| text(&r[2]) == "customers")
        .unwrap();
    assert_eq!(text(&customers[0]), "TABLE");
    assert_eq!(text(&customers[3]), "2", "row estimate");
    assert_eq!(text(&customers[7]), "people", "comment");
    assert!(inventory.iter().any(|r| text(&r[0]) == "VIEW"));
    assert!(inventory.iter().any(|r| text(&r[0]) == "MATERIALIZED VIEW"));
    assert!(inventory.iter().any(|r| text(&r[0]) == "FUNCTION"));

    let columns = run(
        &fx,
        MetadataQuery::Columns {
            schema: DB.into(),
            object: "customers".into(),
        },
    )
    .await;
    assert_eq!(
        names(&columns, 0),
        vec!["id", "name", "mood", "tags", "created", "name_upper"]
    );
    assert_eq!(text(&columns[3][1]), "Array(String)");
    assert_eq!(text(&columns[5][7]), "1", "MATERIALIZED column is computed");
    let orders = run(
        &fx,
        MetadataQuery::Columns {
            schema: DB.into(),
            object: "orders".into(),
        },
    )
    .await;
    let amount = &orders[2];
    assert_eq!(text(&amount[1]), "Decimal(12, 2)");
    assert_eq!(text(&amount[3]), "12");
    assert_eq!(text(&amount[4]), "2");
    assert_eq!(text(&amount[5]), "0", "not nullable");

    let all_columns = run(&fx, MetadataQuery::AllColumns).await;
    assert!(all_columns.len() >= 10);
    assert!(all_columns.iter().all(|r| text(&r[0]) == DB));

    assert!(run(&fx, MetadataQuery::AllForeignKeys).await.is_empty());

    let pks = run(&fx, MetadataQuery::AllPrimaryKeys).await;
    let order_key: Vec<(String, String)> = pks
        .iter()
        .filter(|r| text(&r[1]) == "orders")
        .map(|r| (text(&r[2]), text(&r[3])))
        .collect();
    assert_eq!(
        order_key,
        vec![
            ("customer_id".to_string(), "1".to_string()),
            ("id".to_string(), "2".to_string())
        ]
    );

    let params = run(
        &fx,
        MetadataQuery::ProcedureParameters {
            schema: DB.into(),
            object: "sqlly_tax".into(),
        },
    )
    .await;
    assert_eq!(names(&params, 0), vec!["amount", "rate"]);

    let indexes = run(
        &fx,
        MetadataQuery::Indexes {
            schema: DB.into(),
            table: "customers".into(),
        },
    )
    .await;
    assert_eq!(names(&indexes, 0), vec!["PRIMARY", "name_idx"]);
    assert_eq!(text(&indexes[1][1]), "bloom_filter");
    let index_columns = run(
        &fx,
        MetadataQuery::IndexColumns {
            schema: DB.into(),
            table: "orders".into(),
            index: "PRIMARY".into(),
        },
    )
    .await;
    assert_eq!(names(&index_columns, 0), vec!["customer_id", "id"]);
    let table_index_columns = run(
        &fx,
        MetadataQuery::TableIndexColumns {
            schema: DB.into(),
            table: "customers".into(),
        },
    )
    .await;
    assert_eq!(names(&table_index_columns, 0), vec!["PRIMARY", "name_idx"]);

    let constraints = run(
        &fx,
        MetadataQuery::Constraints {
            schema: DB.into(),
            table: "customers".into(),
        },
    )
    .await;
    assert_eq!(names(&constraints, 0), vec!["PRIMARY", "name_not_blank"]);
    assert_eq!(text(&constraints[1][1]), "CHECK_CONSTRAINT");
    let check = run(
        &fx,
        MetadataQuery::ConstraintDefinition {
            schema: DB.into(),
            table: "customers".into(),
            name: "name_not_blank".into(),
        },
    )
    .await;
    assert!(text(&check[0][0]).contains("name"), "{check:?}");
    let pk = run(
        &fx,
        MetadataQuery::ConstraintDefinition {
            schema: DB.into(),
            table: "orders".into(),
            name: "PRIMARY".into(),
        },
    )
    .await;
    assert_eq!(text(&pk[0][0]), "PRIMARY KEY (customer_id, id)");

    let defaults = run(
        &fx,
        MetadataQuery::ColumnDefaults {
            schema: DB.into(),
            table: "customers".into(),
        },
    )
    .await;
    assert_eq!(names(&defaults, 0), vec!["created", "name_upper"]);
    assert_eq!(text(&defaults[0][1]), "now()");

    let dependents = run(
        &fx,
        MetadataQuery::DependentViews {
            schema: DB.into(),
            table: "orders".into(),
        },
    )
    .await;
    let dependent_names = names(&dependents, 1);
    assert!(
        dependent_names.contains(&"big_orders".to_string()),
        "{dependent_names:?}"
    );
    assert!(
        dependent_names.contains(&"order_totals".to_string()),
        "{dependent_names:?}"
    );

    let general = run(
        &fx,
        MetadataQuery::TableGeneral {
            schema: DB.into(),
            table: "orders".into(),
        },
    )
    .await;
    assert_eq!(text(&general[0][4]), "3", "row count");

    let db_general = run(
        &fx,
        MetadataQuery::DatabaseGeneral {
            database: DB.into(),
        },
    )
    .await;
    assert_eq!(text(&db_general[0][0]), DB);
    assert_eq!(
        text(&db_general[0][7]),
        "Atomic",
        "engine reported as owner"
    );

    let column = run(
        &fx,
        MetadataQuery::ColumnGeneral {
            schema: DB.into(),
            table: "orders".into(),
            column: "amount".into(),
        },
    )
    .await;
    assert_eq!(text(&column[0][4]), "Decimal(12, 2)");

    let object = run(
        &fx,
        MetadataQuery::ObjectGeneral {
            schema: DB.into(),
            object: "big_orders".into(),
        },
    )
    .await;
    assert_eq!(text(&object[0][2]), "VIEW");
    let function = run(
        &fx,
        MetadataQuery::ObjectGeneral {
            schema: DB.into(),
            object: "sqlly_tax".into(),
        },
    )
    .await;
    assert_eq!(text(&function[0][2]), "SQL_SCALAR_FUNCTION");

    let definition = run(
        &fx,
        MetadataQuery::ObjectDefinition {
            schema: DB.into(),
            object: "big_orders".into(),
        },
    )
    .await;
    assert!(text(&definition[0][0]).starts_with("CREATE VIEW"));
    let udf = run(
        &fx,
        MetadataQuery::ObjectDefinition {
            schema: DB.into(),
            object: "sqlly_tax".into(),
        },
    )
    .await;
    assert!(text(&udf[0][0]).starts_with("CREATE FUNCTION"));

    let capability = run(&fx, MetadataQuery::ServerCapability).await;
    assert_eq!(text(&capability[0][1]), "ClickHouse");
    let settings = run(&fx, MetadataQuery::ServerConfiguration).await;
    assert!(settings.iter().any(|r| text(&r[0]) == "max_threads"));
    let files = run(
        &fx,
        MetadataQuery::DatabaseFiles {
            database: DB.into(),
        },
    )
    .await;
    assert_eq!(text(&files[0][1]), "Atomic");
    let permissions = run(&fx, MetadataQuery::ServerPermissions).await;
    assert!(!permissions.is_empty(), "the default user holds grants");
}

#[tokio::test]
async fn admin_tool_queries_run_and_plans_parse() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    let disk = rows(
        &fx.client,
        DiskUsage::databases_query_for(DiskDialect::ClickHouse),
    )
    .await;
    assert!(disk.iter().any(|r| text(&r[0]) == DB), "{disk:?}");
    let disks = rows(
        &fx.client,
        DiskUsage::files_query_for(DiskDialect::ClickHouse).unwrap(),
    )
    .await;
    assert!(disks.iter().any(|r| text(&r[0]) == "default"));

    let mut keys = Vec::new();
    for group in Dashboard::metric_groups_for(DashboardDialect::ClickHouse) {
        for row in rows(&fx.client, group.sql).await {
            keys.push(text(&row[0]));
            assert!(text(&row[1]).parse::<f64>().is_ok(), "{group:?}: {row:?}");
        }
    }
    for key in [
        "connections_active",
        "queries_total",
        "sessions_running",
        "connections_max",
        "uptime_seconds",
        "storage_total_mb",
        "objects_total",
        "indexes_total",
    ] {
        assert!(
            keys.contains(&key.to_string()),
            "{key} missing from {keys:?}"
        );
    }

    for sql in [
        ActivityMonitor::sessions_query_for(ActivityDialect::ClickHouse),
        ActivityMonitor::blocking_query_for(ActivityDialect::ClickHouse),
        ActivityMonitor::expensive_queries_query_for(ActivityDialect::ClickHouse),
        ActivityMonitor::waits_query_for(ActivityDialect::ClickHouse),
    ] {
        let _ = rows(&fx.client, sql).await;
    }
    let diagnostics = ActivityMonitor::diagnostic_sql_for(ActivityDialect::ClickHouse, 7);
    for statement in diagnostics.split(';').filter(|s| !s.trim().is_empty()) {
        let _ = rows(&fx.client, statement).await;
    }
    // Killing a session id nothing runs under is a no-op, not an error.
    let kill = ActivityMonitor::kill_session_sql_for(ActivityDialect::ClickHouse, 7);
    execute_batch_on_client(&fx.client, &kill).await.unwrap();

    let probe = sqlly_services::server_info::probe_sql(DatabaseType::ClickHouse).unwrap();
    let info = rows(&fx.client, probe).await;
    assert_eq!(text(&info[0][0]), "ClickHouse");
    assert_eq!(text(&info[0][6]), DB);

    let optimize = maintenance_sql(
        DatabaseType::ClickHouse,
        MaintenanceOp::Optimize,
        &MaintenanceTarget::Table {
            schema: Some(DB.into()),
            name: "orders".into(),
        },
        &[],
    )
    .unwrap();
    assert_eq!(optimize, format!("OPTIMIZE TABLE `{DB}`.`orders` FINAL;"));
    execute_batch_on_client(&fx.client, &optimize)
        .await
        .unwrap();

    let query = format!(
        "SELECT customer_id, sum(amount) FROM {DB}.orders WHERE customer_id = 1 GROUP BY customer_id"
    );
    for capture in [
        estimated_capture_sql(PlanDialect::ClickHouse, &query),
        explain_capture_sql(PlanDialect::ClickHouse, &query),
    ] {
        let doc = rows(&fx.client, &capture).await;
        let plan = &parse_captured_plan(PlanDialect::ClickHouse, &text(&doc[0][0])).unwrap()[0];
        assert!(plan.nodes.len() >= 2, "{plan:?}");
        let read = plan
            .nodes
            .iter()
            .find(|n| n.logical_op == "ReadFromMergeTree")
            .expect("a read step");
        assert_eq!(read.physical_op, format!("ReadFromMergeTree {DB}.orders"));
        assert!(
            read.output_list
                .iter()
                .any(|l| l.starts_with("PrimaryKey:")),
            "{:?}",
            read.output_list
        );
        assert!(
            read.warnings.is_empty(),
            "the key prunes: {:?}",
            read.warnings
        );
    }
    let _ = fx.profile;
}

// ── Tool coverage added with the ClickHouse tool pass ──────────────────────

/// A scratch table for the tests that write, created fresh per test.
async fn scratch_table(fx: &Fixture, name: &str, columns: &str, order_by: &str) {
    execute_batch_on_client(
        &fx.client,
        &format!(
            "DROP TABLE IF EXISTS {DB}.{name};\nCREATE TABLE {DB}.{name} ({columns}) ENGINE = MergeTree ORDER BY {order_by}"
        ),
    )
    .await
    .unwrap();
}

/// Query Insights: the query log answers the top-queries and recent-failures
/// SQL after a flush, and a failed statement shows up with its exception.
#[tokio::test]
async fn query_insights_read_the_query_log() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    use sqlly_services::{AdminToolsService, DefaultAdminToolsService, QueryStoreRank};
    let admin = DefaultAdminToolsService;
    let _ = rows(
        &fx.client,
        &format!("SELECT count() FROM {DB}.orders WHERE amount > 1"),
    )
    .await;
    let failure = query_rows_on_client(
        &fx.client,
        &format!("SELECT * FROM {DB}.sqlly_no_such_table"),
    )
    .await;
    assert!(failure.is_err());
    execute_batch_on_client(&fx.client, "SYSTEM FLUSH LOGS")
        .await
        .unwrap();

    for rank in [
        QueryStoreRank::AvgDuration,
        QueryStoreRank::ExecutionCount,
        QueryStoreRank::AvgLogicalReads,
    ] {
        for hours in [None, Some(24)] {
            let sql = admin
                .query_insights_top(DatabaseType::ClickHouse, DB, 50, rank, hours)
                .unwrap();
            let set = query_rows_on_client(&fx.client, &sql)
                .await
                .unwrap_or_else(|e| panic!("insights SQL rejected:\n{sql}\n{e}"));
            let names: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                vec![
                    "query_id",
                    "query_sql_text",
                    "avg_duration_ms",
                    "count_executions",
                    "avg_read_rows",
                    "avg_read_mb",
                    "peak_memory_mb",
                    "total_rows",
                    "last_seen"
                ]
            );
            assert!(!set.rows.is_empty(), "{rank:?}/{hours:?}");
            assert!(
                set.rows.iter().all(|r| !text(&r[1]).is_empty()),
                "every row carries its normalized text: {rank:?}/{hours:?}"
            );
        }
    }
    // The top-N by any rank need not include the fixture query once the log
    // is busy; a wide window must.
    let wide = admin
        .query_insights_top(
            DatabaseType::ClickHouse,
            DB,
            1000,
            QueryStoreRank::ExecutionCount,
            Some(24),
        )
        .unwrap();
    let logged = rows(&fx.client, &wide).await;
    assert!(
        logged
            .iter()
            .any(|r| text(&r[1]).contains("orders WHERE amount > ?")),
        "the fixture query is in the log, normalized"
    );
    let failures = admin
        .query_insights_failures(DatabaseType::ClickHouse, 50, Some(1))
        .unwrap();
    let set = rows(&fx.client, &failures).await;
    assert!(
        set.iter()
            .any(|r| text(&r[1]).contains("sqlly_no_such_table") && !text(&r[3]).is_empty()),
        "{set:?}"
    );
}

/// Security: every listing runs and the server's own CREATE / GRANT text
/// is readable; the scripted CREATE / GRANT statements are accepted when
/// the connected user holds ACCESS MANAGEMENT (the docker image's `default`
/// user does not, in which case the server's answer must be a privilege
/// error, never a syntax error).
#[tokio::test]
async fn security_listings_and_scripts_round_trip() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    use sqlly_services::admin::security_sql::clickhouse as ch;
    use sqlly_services::{AdminToolsService, DefaultAdminToolsService};
    let admin = DefaultAdminToolsService;
    let db = DatabaseType::ClickHouse;

    let users = rows(&fx.client, &admin.security_query_for(db, "users").unwrap()).await;
    let me = users
        .iter()
        .find(|r| text(&r[0]) == "default")
        .expect("the connected user is listed");
    assert_eq!(me.len(), 10, "{me:?}");
    assert!(!text(&me[9]).is_empty(), "storage: {me:?}");
    for kind in ["roles", "grants", "role-grants", "profiles"] {
        let _ = rows(&fx.client, &admin.security_query_for(db, kind).unwrap()).await;
    }
    let grants = rows(&fx.client, &admin.security_query_for(db, "grants").unwrap()).await;
    assert!(
        grants
            .iter()
            .any(|r| text(&r[0]) == "default" && text(&r[2]) == "SELECT"),
        "{grants:?}"
    );
    // The server's own CREATE / GRANT text needs SHOW USERS; without it the
    // dialog falls back to the script built from the catalog rows, which
    // must render the connected user's SELECT grant.
    let show_grants = admin
        .security_show_grants_sql_for(db, "default", "")
        .unwrap();
    match query_rows_on_client(&fx.client, &ch::show_create_user_sql("default")).await {
        Ok(create) => {
            assert!(
                text(&create.rows[0][0]).starts_with("CREATE USER default"),
                "{create:?}"
            );
            let shown = rows(&fx.client, &show_grants).await;
            assert!(
                shown.iter().any(|r| text(&r[0]).starts_with("GRANT ")),
                "{shown:?}"
            );
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("privileges") || message.contains("ACCESS"),
                "SHOW CREATE USER must parse; got: {message}"
            );
            let mine: Vec<ch::GrantRow> = grants
                .iter()
                .filter(|r| text(&r[0]) == "default")
                .map(|r| ch::GrantRow {
                    access_type: text(&r[2]),
                    database: text(&r[3]),
                    table: text(&r[4]),
                    column: text(&r[5]),
                    is_partial_revoke: text(&r[6]) == "1",
                    grant_option: text(&r[7]) == "1",
                })
                .collect();
            let script = ch::principal_script("default", false, &mine, &[]);
            assert!(
                script.contains("CREATE USER IF NOT EXISTS `default`"),
                "{script}"
            );
            assert!(
                script.contains("GRANT SELECT ON *.* TO `default`;"),
                "{script}"
            );
        }
    }

    let script = [
        ch::create_role_sql("sqlly_reader"),
        ch::grant_statement_sql(
            "sqlly_reader",
            &ch::GrantRow {
                access_type: "SELECT".into(),
                database: DB.into(),
                table: String::new(),
                column: String::new(),
                is_partial_revoke: false,
                grant_option: false,
            },
        ),
        ch::create_user_sql(
            "sqlly_probe",
            Some("pr0be-pw"),
            &ch::HostRestriction::Ip("10.0.0.0/8".into()),
            &[],
        ),
        ch::grant_role_sql("sqlly_reader", "sqlly_probe", false),
        ch::grant_statement_sql(
            "sqlly_probe",
            &ch::GrantRow {
                access_type: "INSERT".into(),
                database: DB.into(),
                table: "orders".into(),
                column: String::new(),
                is_partial_revoke: false,
                grant_option: true,
            },
        ),
    ]
    .join(";\n");
    match execute_batch_on_client(&fx.client, &script).await {
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("privileges") || message.contains("ACCESS"),
                "scripted CREATE / GRANT statements must parse; got: {message}\n{script}"
            );
            eprintln!("skipping the write round-trip (no ACCESS MANAGEMENT): {message}");
        }
        Ok(()) => {
            let users = rows(&fx.client, &admin.security_query_for(db, "users").unwrap()).await;
            let probe = users
                .iter()
                .find(|r| text(&r[0]) == "sqlly_probe")
                .expect("scripted user listed");
            assert!(text(&probe[1]).contains("sha256_password"), "{probe:?}");
            assert_eq!(text(&probe[2]), "10.0.0.0/8");
            let roles = rows(&fx.client, &admin.security_query_for(db, "roles").unwrap()).await;
            assert!(roles.iter().any(|r| text(&r[0]) == "sqlly_reader"));
            let grants = rows(&fx.client, &admin.security_query_for(db, "grants").unwrap()).await;
            assert!(
                grants.iter().any(|r| text(&r[0]) == "sqlly_probe"
                    && text(&r[2]) == "INSERT"
                    && text(&r[3]) == DB
                    && text(&r[4]) == "orders"
                    && text(&r[7]) == "1"),
                "{grants:?}"
            );
            assert!(grants.iter().any(|r| text(&r[1]) == "sqlly_reader"
                && text(&r[2]) == "SELECT"
                && text(&r[3]) == DB));
            let role_grants = rows(
                &fx.client,
                &admin.security_query_for(db, "role-grants").unwrap(),
            )
            .await;
            assert!(
                role_grants
                    .iter()
                    .any(|r| text(&r[0]) == "sqlly_probe" && text(&r[2]) == "sqlly_reader")
            );
            let shown = rows(
                &fx.client,
                &admin
                    .security_show_grants_sql_for(db, "sqlly_probe", "")
                    .unwrap(),
            )
            .await;
            assert!(
                shown.iter().any(|r| text(&r[0]).contains("sqlly_reader")),
                "{shown:?}"
            );
            execute_batch_on_client(
                &fx.client,
                &format!(
                    "{};\n{}",
                    ch::drop_user_sql("sqlly_probe"),
                    ch::drop_role_sql("sqlly_reader")
                ),
            )
            .await
            .unwrap();
        }
    }
}

/// Structure editor: the snapshot loads through the metadata dialect, and
/// the rendered ALTER script (add column, add index, rename) is accepted.
#[tokio::test]
async fn structure_editor_alter_script_executes() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    use sqlly_services::alter::{
        ColumnDef, DraftColumn, IndexDef, RenderOptions, TableDraft, diff, render,
    };
    use sqlly_services::{Services, load_table_snapshot};
    scratch_table(
        &fx,
        "alter_scratch",
        "id UInt32, note String, amount Decimal(10, 2)",
        "id",
    )
    .await;
    let services = Services::new();
    let snapshot = load_table_snapshot(
        &*services.queries,
        &fx.profile,
        DB,
        Some(DB),
        "alter_scratch",
    )
    .await
    .expect("snapshot");
    assert_eq!(
        snapshot
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["id", "note", "amount"]
    );
    assert_eq!(snapshot.columns[2].data_type, "Decimal(10, 2)");
    assert_eq!(snapshot.primary_key, vec!["id".to_string()]);

    let mut draft = TableDraft {
        schema: snapshot.schema.clone(),
        table: snapshot.table.clone(),
        columns: snapshot
            .columns
            .iter()
            .map(|c| DraftColumn {
                origin: Some(c.name.clone()),
                def: c.clone(),
            })
            .collect(),
        primary_key: snapshot.primary_key.clone(),
        pk_constraint_name: None,
        indexes: snapshot.indexes.clone(),
        foreign_keys: Vec::new(),
        table_comment: snapshot.table_comment.clone(),
        column_comments: snapshot.column_comments.clone(),
        source_ddl: None,
    };
    draft.columns[1].def.name = "comment".into();
    draft.columns.push(DraftColumn {
        origin: None,
        def: ColumnDef {
            name: "tags".into(),
            data_type: "Array(String)".into(),
            nullable: false,
            identity: false,
            default: Some("[]".into()),
            default_constraint_name: None,
        },
    });
    draft.columns.push(DraftColumn {
        origin: None,
        def: ColumnDef {
            name: "region".into(),
            data_type: "String".into(),
            nullable: true,
            identity: false,
            default: None,
            default_constraint_name: None,
        },
    });
    draft.indexes.push(IndexDef {
        name: "idx_comment".into(),
        columns: vec!["comment".into()],
        unique: false,
        include: vec![],
        filter: None,
        method: Some("bloom_filter".into()),
        fillfactor: None,
    });
    let set = diff(&snapshot, &draft);
    let script = render(
        DatabaseType::ClickHouse,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .unwrap();
    assert!(
        script.sql.contains("RENAME COLUMN `note` TO `comment`"),
        "{}",
        script.sql
    );
    assert!(
        script.sql.contains("ADD COLUMN `region` Nullable(String)"),
        "{}",
        script.sql
    );
    assert!(
        script.sql.contains("ADD INDEX `idx_comment`"),
        "{}",
        script.sql
    );
    execute_batch_on_client(&fx.client, &script.sql)
        .await
        .unwrap_or_else(|e| panic!("ALTER script rejected:\n{}\n{e}", script.sql));
    let columns = rows(
        &fx.client,
        &format!("SELECT name, type FROM system.columns WHERE database = '{DB}' AND table = 'alter_scratch' ORDER BY position"),
    )
    .await;
    assert_eq!(
        columns.iter().map(|r| text(&r[0])).collect::<Vec<_>>(),
        vec!["id", "comment", "amount", "tags", "region"]
    );
    assert_eq!(text(&columns[4][1]), "Nullable(String)");
    let indexes = rows(
        &fx.client,
        &format!("SELECT name, type FROM system.data_skipping_indices WHERE database = '{DB}' AND table = 'alter_scratch'"),
    )
    .await;
    assert_eq!(text(&indexes[0][0]), "idx_comment");
    assert_eq!(text(&indexes[0][1]), "bloom_filter");
}

/// Data transfer: rows from a SQLite file land in a new MergeTree table.
#[tokio::test]
async fn transfer_sink_copies_a_sqlite_table_into_clickhouse() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    use sqlly_core::{SecretProvider, SecretRef};
    use sqlly_services::transfer::plan::{
        NameCasing, TransferColumn, TransferMode, TransferPlan, TransferTable,
    };
    use sqlly_services::{Services, TransferCancel, TransferProgress, open_sink, run_transfer};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.sqlite");
    let mut source_profile = ConnectionProfile::sql_login(
        "sqlite-source",
        "localhost",
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    source_profile.database_type = DatabaseType::Sqlite;
    source_profile.database = Some(path.to_string_lossy().into_owned());
    let sqlite = sqlly_engines::sqlite::open_client(&source_profile)
        .await
        .unwrap();
    for sql in [
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, price REAL, note TEXT)",
        "INSERT INTO items VALUES (1, 'amber', 1.5, NULL), (2, 'o''brien\\x', 2.25, 'n'), (3, 'cedar', 0, '')",
    ] {
        sqlly_engines::sqlite::execute_params_on_client(&sqlite, sql, &[])
            .await
            .unwrap();
    }

    execute_batch_on_client(&fx.client, &format!("DROP TABLE IF EXISTS {DB}.items"))
        .await
        .unwrap();
    let plan = TransferPlan {
        source: DatabaseType::Sqlite,
        target: DatabaseType::ClickHouse,
        target_schema: Some(DB.into()),
        mode: TransferMode::CreateTable,
        casing: NameCasing::AsIs,
        tables: vec![TransferTable {
            schema: None,
            name: "items".into(),
            columns: vec![
                TransferColumn {
                    name: "id".into(),
                    source_type: "INTEGER".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: Some(1),
                },
                TransferColumn {
                    name: "name".into(),
                    source_type: "TEXT".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "price".into(),
                    source_type: "REAL".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "note".into(),
                    source_type: "TEXT".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
            ],
            depends_on: Vec::new(),
        }],
    };
    let mut sink = open_sink(&fx.profile).await.expect("ClickHouse sink");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<TransferProgress>(64);
    let services = Services::new();
    let outcome = run_transfer(
        &services,
        &source_profile,
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    while rx.try_recv().is_ok() {}
    assert!(outcome.error.is_none(), "{:?}", outcome);
    assert_eq!(outcome.tables, vec![("items".to_string(), 3)]);

    let copied = rows(
        &fx.client,
        &format!("SELECT id, name, price, note FROM {DB}.items ORDER BY id"),
    )
    .await;
    assert_eq!(copied.len(), 3);
    assert_eq!(text(&copied[1][1]), "o'brien\\x");
    assert_eq!(text(&copied[0][3]), "", "NULL note arrives as NULL");
    assert!(matches!(copied[0][3], CellValue::Null), "{:?}", copied[0]);
    let engine = rows(
        &fx.client,
        &format!("SELECT engine, sorting_key FROM system.tables WHERE database = '{DB}' AND name = 'items'"),
    )
    .await;
    assert_eq!(text(&engine[0][0]), "MergeTree");
    assert_eq!(text(&engine[0][1]), "id");
}

/// Data compare: the ClickHouse sync script (INSERT, lightweight DELETE,
/// ALTER TABLE … UPDATE) is accepted and leaves the table in sync.
#[tokio::test]
async fn data_compare_sync_script_executes() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    use sqlly_core::ResultColumn;
    use sqlly_services::datacompare::script::{
        SyncDirection, SyncScriptRequest, generate_sync_script,
    };
    use sqlly_services::datacompare::{ChangedPair, DataDiff};
    scratch_table(
        &fx,
        "sync_scratch",
        "id UInt32, name String, active Bool",
        "id",
    )
    .await;
    execute_batch_on_client(
        &fx.client,
        &format!("INSERT INTO {DB}.sync_scratch VALUES (2, 'gone', false), (3, 'b', true)"),
    )
    .await
    .unwrap();
    let columns: Vec<ResultColumn> = [("id", "UInt32"), ("name", "String"), ("active", "Bool")]
        .iter()
        .map(|(name, ty)| ResultColumn {
            name: (*name).into(),
            data_type: (*ty).into(),
            nullable: false,
        })
        .collect();
    let diff = DataDiff {
        only_left: vec![vec![
            CellValue::I64(1),
            CellValue::String("O'Brien".into()),
            CellValue::Bool(true),
        ]],
        only_right: vec![vec![
            CellValue::I64(2),
            CellValue::String("gone".into()),
            CellValue::Bool(false),
        ]],
        changed: vec![ChangedPair {
            key: vec![CellValue::I64(3)],
            left: vec![
                CellValue::I64(3),
                CellValue::String("a".into()),
                CellValue::Bool(true),
            ],
            right: vec![
                CellValue::I64(3),
                CellValue::String("b".into()),
                CellValue::Bool(true),
            ],
            changed_columns: vec![1],
        }],
        unchanged: 0,
        scanned_left: 0,
        scanned_right: 0,
        truncated: None,
    };
    let destination = format!("`{DB}`.`sync_scratch`");
    let script = generate_sync_script(&SyncScriptRequest {
        db: DatabaseType::ClickHouse,
        destination_table: &destination,
        columns: &columns,
        key_indexes: &[0],
        diff: &diff,
        direction: SyncDirection::LeftToRight,
        header_note: "live",
    });
    // Mutations are asynchronous by default; wait for them so the check
    // below sees the update.
    execute_batch_on_client(&fx.client, "SET mutations_sync = 2")
        .await
        .unwrap();
    execute_batch_on_client(&fx.client, &script.sql)
        .await
        .unwrap_or_else(|e| panic!("sync script rejected:\n{}\n{e}", script.sql));
    let after = rows(
        &fx.client,
        &format!("SELECT id, name FROM {DB}.sync_scratch ORDER BY id"),
    )
    .await;
    assert_eq!(
        after
            .iter()
            .map(|r| (text(&r[0]), text(&r[1])))
            .collect::<Vec<_>>(),
        vec![
            ("1".to_string(), "O'Brien".to_string()),
            ("3".to_string(), "a".to_string())
        ]
    );
}

/// Backup: the statement the dialog scripts is well-formed; the container
/// has no `backups` disk, so the server's answer names the disk.
#[tokio::test]
async fn backup_statement_is_parsed_by_the_server() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    use sqlly_services::admin::backup::BackupManager;
    use sqlly_services::admin::restore::RestoreManager;
    let backup = BackupManager::clickhouse_backup_sql(DB, "sqlly_probe.zip", true);
    match execute_batch_on_client(&fx.client, &backup).await {
        Ok(()) => {
            let restore = RestoreManager::clickhouse_restore_sql(DB, "sqlly_probe.zip", true);
            execute_batch_on_client(&fx.client, &restore)
                .await
                .unwrap_or_else(|e| panic!("restore rejected:\n{restore}\n{e}"));
        }
        Err(error) => {
            let message = error.to_string().to_ascii_lowercase();
            assert!(
                message.contains("backups") || message.contains("disk"),
                "the statement parsed but the disk is missing: {error}"
            );
        }
    }
}
