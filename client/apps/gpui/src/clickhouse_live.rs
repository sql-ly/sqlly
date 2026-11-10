//! The ClickHouse SQL this crate generates for its tool panes — the Schema
//! Compare collector, View Dependencies / Find Usages, and the Test Data
//! Generator's column probe and INSERT script — run against a real server.
//! These generators live in private modules, so the checks are unit tests
//! here rather than an integration test.
//!
//! Opt-in: set `SQLLY_CLICKHOUSE_URL` (for example
//! `http://default:sqlly@127.0.0.1:8123/default`); every test self-skips
//! without it.

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, ResultColumn};
use sqlly_engines::clickhouse::{
    ClickHouseClient, execute_batch_on_client, live_profile_from_env, open_client,
    query_rows_on_client,
};
use sqlly_services::admin::schema_plan::{
    DeployOptions, DifferenceStatus, compare_objects, plan_deploy,
};

use crate::object_dependencies::{dependencies_sql, usages_sql};
use crate::schema_compare::{collect_objects_sql, objects_from_outcome};
use crate::test_data_generator::{columns_query, insert_script, parse_columns};
use crate::tool_dialog::{ToolQueryOutcome, ToolQuerySet};

const DB: &str = "sqlly_gpui_ch";
const DB2: &str = "sqlly_gpui_ch2";

fn schema_sql(db: &str) -> String {
    format!(
        "DROP DATABASE IF EXISTS {db};
CREATE DATABASE {db};
CREATE TABLE {db}.customers (id UInt32, name String, mood Enum8('sad' = 1, 'ok' = 2, 'happy' = 3), tags Array(String), created DateTime DEFAULT now(), name_upper String MATERIALIZED upper(name), CONSTRAINT name_not_blank CHECK name <> '', INDEX name_idx name TYPE bloom_filter GRANULARITY 1) ENGINE = MergeTree ORDER BY id;
CREATE TABLE {db}.orders (id UInt32, customer_id UInt32, amount Decimal(12, 2), placed_at DateTime64(3, 'UTC')) ENGINE = MergeTree PARTITION BY toYYYYMM(placed_at) ORDER BY (customer_id, id);
CREATE VIEW {db}.big_orders AS SELECT o.id, c.name, o.amount FROM {db}.orders o JOIN {db}.customers c ON c.id = o.customer_id WHERE o.amount > 100;
CREATE MATERIALIZED VIEW {db}.order_totals ENGINE = SummingMergeTree ORDER BY customer_id AS SELECT customer_id, sum(amount) AS total FROM {db}.orders GROUP BY customer_id;
CREATE DICTIONARY {db}.customer_names (id UInt64, name String) PRIMARY KEY id SOURCE(CLICKHOUSE(TABLE 'customers' DB '{db}')) LAYOUT(FLAT()) LIFETIME(0);
INSERT INTO {db}.customers (id, name, mood, tags) VALUES (1, 'Ada', 'happy', ['vip']), (2, 'Grace', 'ok', []);
INSERT INTO {db}.orders VALUES (1, 1, 150.00, '2024-01-02 10:00:00'), (2, 2, 20.50, '2024-01-03 11:00:00')"
    )
}

struct Fixture {
    client: ClickHouseClient,
}

/// The fixture database is built once per test process: the tests run in
/// parallel and only add rows or work in their own second database.
static FIXTURE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn fixture() -> Option<Fixture> {
    let mut profile: ConnectionProfile = live_profile_from_env("clickhouse-gpui")?;
    FIXTURE
        .get_or_init(|| async {
            let bootstrap = open_client(&profile).await.expect("connect");
            execute_batch_on_client(&bootstrap, &schema_sql(DB))
                .await
                .expect("fixture schema");
        })
        .await;
    profile.database = Some(DB.to_string());
    let client = open_client(&profile).await.expect("connect to fixture db");
    Some(Fixture { client })
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

async fn rows(client: &ClickHouseClient, sql: &str) -> Vec<Vec<CellValue>> {
    match query_rows_on_client(client, sql).await {
        Ok(set) => set.rows,
        Err(e) => panic!("ClickHouse rejected generated SQL:\n{sql}\n{e}"),
    }
}

/// Run a `;`-separated collector batch statement by statement, shaped as
/// the dialog's query outcome.
async fn collect(client: &ClickHouseClient, batch: &str) -> ToolQueryOutcome {
    let mut outcome = ToolQueryOutcome::default();
    for statement in batch.split(";\n").filter(|s| !s.trim().is_empty()) {
        let rows = rows(client, statement.trim().trim_end_matches(';')).await;
        outcome.sets.push(ToolQuerySet {
            columns: Vec::<ResultColumn>::new(),
            rows,
        });
    }
    outcome
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    crate::ensure_test_tokio_runtime();
    crate::tokio_handle().block_on(future)
}

#[test]
fn schema_compare_collects_the_fixture_and_diffs_two_databases() {
    block_on(async {
        let Some(fx) = fixture().await else {
            eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
            return;
        };
        let source_sql = collect_objects_sql(DatabaseType::ClickHouse, DB).unwrap();
        let source = objects_from_outcome(&collect(&fx.client, &source_sql).await);
        let kinds = |kind: &str| -> Vec<String> {
            source
                .iter()
                .filter(|o| o.kind == kind)
                .map(|o| o.name.clone())
                .collect()
        };
        assert_eq!(kinds("TABLE"), vec!["customers", "orders"]);
        assert_eq!(kinds("VIEW"), vec!["big_orders"]);
        assert_eq!(kinds("MATERIALIZED VIEW"), vec!["order_totals"]);
        assert_eq!(kinds("DICTIONARY"), vec!["customer_names"]);
        assert_eq!(kinds("INDEX"), vec!["customers.name_idx"]);
        assert_eq!(
            kinds("PRIMARY KEY"),
            vec!["customers.PRIMARY", "orders.PRIMARY"]
        );
        assert_eq!(kinds("CHECK CONSTRAINT"), vec!["customers.name_not_blank"]);
        let customers = source.iter().find(|o| o.name == "customers").unwrap();
        assert!(
            customers
                .definition
                .contains("name_upper String MATERIALIZED upper(name)"),
            "{}",
            customers.definition
        );
        assert!(
            customers
                .definition
                .contains("created DateTime DEFAULT now()"),
            "{}",
            customers.definition
        );
        assert!(
            customers.definition.contains(" ENGINE = MergeTree"),
            "{}",
            customers.definition
        );
        let orders_pk = source.iter().find(|o| o.name == "orders.PRIMARY").unwrap();
        assert_eq!(orders_pk.definition, "customer_id, id");
        assert!(source.iter().all(|o| o.schema == DB));

        // A second database: one column wider, the plain view missing.
        execute_batch_on_client(&fx.client, &schema_sql(DB2))
            .await
            .unwrap();
        execute_batch_on_client(
            &fx.client,
            &format!(
                "ALTER TABLE {DB2}.orders ADD COLUMN note String;\nDROP VIEW {DB2}.big_orders"
            ),
        )
        .await
        .unwrap();
        let target_sql = collect_objects_sql(DatabaseType::ClickHouse, DB2).unwrap();
        let mut target = objects_from_outcome(&collect(&fx.client, &target_sql).await);
        // Compare by object identity: the database name is the schema.
        for o in &mut target {
            o.schema = DB.to_string();
            o.definition = o.definition.replace(DB2, DB);
        }
        let differences = compare_objects(
            &source,
            &target,
            DatabaseType::ClickHouse,
            DatabaseType::ClickHouse,
        );
        let orders = differences
            .iter()
            .find(|d| d.object.name == "orders" && d.object.kind == "TABLE")
            .expect("orders differs");
        assert_eq!(orders.status, DifferenceStatus::Changed);
        let view = differences
            .iter()
            .find(|d| d.object.name == "big_orders")
            .expect("view missing in target");
        assert_eq!(view.status, DifferenceStatus::MissingInTarget);
        assert!(
            !differences
                .iter()
                .any(|d| d.object.name == "customers" && d.object.kind == "TABLE"),
            "{differences:?}"
        );

        // The deploy plan recreates the view with the server's own CREATE
        // statement, which ClickHouse accepts against the target.
        let plan = plan_deploy(
            &differences,
            DatabaseType::ClickHouse,
            DatabaseType::ClickHouse,
            DeployOptions::default(),
        );
        let view_step = plan.steps.iter().find(|s| s.kind == "VIEW").unwrap();
        assert!(view_step.runnable, "{view_step:?}");
        let sql = view_step.sql.replace(&format!("{DB}."), &format!("{DB2}."));
        execute_batch_on_client(&fx.client, &sql)
            .await
            .unwrap_or_else(|e| panic!("view step rejected:\n{sql}\n{e}"));
        let views = rows(
            &fx.client,
            &format!("SELECT name FROM system.tables WHERE database = '{DB2}' AND engine = 'View'"),
        )
        .await;
        assert_eq!(text(&views[0][0]), "big_orders");
        execute_batch_on_client(&fx.client, &format!("DROP DATABASE {DB2}"))
            .await
            .unwrap();
    });
}

#[test]
fn dependencies_and_usages_read_the_catalog() {
    block_on(async {
        let Some(fx) = fixture().await else {
            eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
            return;
        };
        let sql = dependencies_sql(DatabaseType::ClickHouse, DB, "orders").unwrap();
        let deps = rows(&fx.client, &sql).await;
        let used_by: Vec<(String, String)> = deps
            .iter()
            .filter(|r| text(&r[0]) == "Used by")
            .map(|r| (text(&r[1]), text(&r[2])))
            .collect();
        assert!(
            used_by
                .iter()
                .any(|(o, t)| o == &format!("`{DB}`.`order_totals`") && t == "MATERIALIZED VIEW"),
            "{used_by:?}"
        );
        assert!(
            used_by
                .iter()
                .any(|(o, t)| o == &format!("`{DB}`.`big_orders`") && t.contains("VIEW")),
            "{used_by:?}"
        );
        // The dictionary reads customers; a materialized view uses orders.
        let view_deps = rows(
            &fx.client,
            &dependencies_sql(DatabaseType::ClickHouse, DB, "order_totals").unwrap(),
        )
        .await;
        assert!(
            view_deps
                .iter()
                .any(|r| text(&r[0]) == "Uses" && text(&r[1]) == format!("`{DB}`.`orders`")),
            "{view_deps:?}"
        );
        let usages = rows(
            &fx.client,
            &usages_sql(DatabaseType::ClickHouse, DB, "customers").unwrap(),
        )
        .await;
        let objects: Vec<String> = usages.iter().map(|r| text(&r[0])).collect();
        assert!(
            objects.contains(&format!("`{DB}`.`big_orders`")),
            "{objects:?}"
        );
        assert!(
            objects.contains(&format!("`{DB}`.`customer_names`")),
            "{objects:?}"
        );
        assert!(usages.iter().all(|r| text(&r[2]).len() <= 200));
    });
}

#[test]
fn test_data_generator_probes_columns_and_its_insert_runs() {
    block_on(async {
        let Some(fx) = fixture().await else {
            eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
            return;
        };
        let probe = columns_query(DatabaseType::ClickHouse, DB, "customers").unwrap();
        let columns = parse_columns(&rows(&fx.client, &probe).await);
        let names: Vec<&str> = columns.iter().map(|(c, _)| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["id", "name", "mood", "tags", "created", "name_upper"]
        );
        let by_name = |name: &str| columns.iter().find(|(c, _)| c.name == name).unwrap();
        assert!(
            by_name("name_upper").0.skip,
            "MATERIALIZED column is skipped"
        );
        assert!(
            !by_name("created").0.skip,
            "DEFAULT column still accepts values"
        );
        assert_eq!(by_name("mood").0.choices, vec!["sad", "ok", "happy"]);
        assert_eq!(by_name("tags").0.fixed_literal.as_deref(), Some("[]"));
        assert!(!by_name("id").0.nullable);

        let before = rows(&fx.client, &format!("SELECT count() FROM {DB}.customers")).await;
        let script = insert_script(
            DatabaseType::ClickHouse,
            DB,
            "customers",
            &columns,
            25,
            0,
            42,
        )
        .unwrap();
        execute_batch_on_client(&fx.client, &script)
            .await
            .unwrap_or_else(|e| panic!("generated INSERT rejected:\n{script}\n{e}"));
        let after = rows(&fx.client, &format!("SELECT count() FROM {DB}.customers")).await;
        assert_eq!(
            text(&after[0][0]).parse::<i64>().unwrap()
                - text(&before[0][0]).parse::<i64>().unwrap(),
            25
        );
        let moods = rows(
            &fx.client,
            &format!("SELECT DISTINCT toString(mood) FROM {DB}.customers ORDER BY 1"),
        )
        .await;
        assert!(
            moods
                .iter()
                .all(|r| matches!(text(&r[0]).as_str(), "sad" | "ok" | "happy"))
        );
    });
}
