//! Every DuckDB-specific SQL generator in the service layer, run against a
//! real (bundled, hermetic) DuckDB file: the catalog SQL behind the explorer
//! and IntelliSense, the disk-usage and dashboard queries, the server-info
//! probe, and plan capture + parsing. A generator whose SQL DuckDB rejects
//! fails here instead of in the app.

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_engines::duckdb::{
    DuckDbClient, execute_batch_on_client, open_client, query_rows_on_client,
};
use sqlly_intel::metadata::MetadataQuery;
use sqlly_intel::metadata::dialect::sql_for_engine;
use sqlly_services::admin::disk_usage::{DiskDialect, DiskUsage};
use sqlly_services::plans::{
    PlanDialect, estimated_capture_sql, explain_capture_sql, parse_captured_plan,
};
use sqlly_services::{Dashboard, DashboardDialect};

const SCHEMA_SQL: &str = "
CREATE SCHEMA sales;
CREATE SEQUENCE sales.order_seq START 100 INCREMENT 5;
CREATE TYPE sales.mood AS ENUM ('sad', 'ok', 'happy');
CREATE TABLE sales.customers (id INTEGER PRIMARY KEY, name VARCHAR NOT NULL, mood sales.mood, tags VARCHAR[], CONSTRAINT customers_name_unique UNIQUE (name));
CREATE TABLE sales.orders (id INTEGER DEFAULT nextval('sales.order_seq') PRIMARY KEY, customer_id INTEGER REFERENCES sales.customers(id), amount DECIMAL(12,2) CHECK (amount >= 0), placed_at TIMESTAMP DEFAULT current_timestamp);
CREATE INDEX orders_customer_idx ON sales.orders (customer_id, placed_at);
CREATE VIEW sales.big_orders AS SELECT o.id, c.name, o.amount FROM sales.orders o JOIN sales.customers c ON c.id = o.customer_id WHERE o.amount > 100;
CREATE MACRO sales.tax(amount, rate := 0.2) AS amount * rate;
CREATE MACRO sales.recent(n) AS TABLE SELECT * FROM sales.orders ORDER BY placed_at DESC LIMIT n;
INSERT INTO sales.customers VALUES (1, 'Ada', 'happy', ['vip']), (2, 'Grace', 'ok', NULL);
INSERT INTO sales.orders (customer_id, amount) VALUES (1, 150.00), (2, 20.50), (1, 999.99);
";

struct Fixture {
    _dir: tempfile::TempDir,
    profile: ConnectionProfile,
    client: DuckDbClient,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut profile = ConnectionProfile::sql_login(
        "duckdb-dialects",
        dir.path()
            .join("dialects.duckdb")
            .to_string_lossy()
            .to_string(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = DatabaseType::DuckDb;
    let client = open_client(&profile).await.unwrap();
    execute_batch_on_client(&client, SCHEMA_SQL).await.unwrap();
    Fixture {
        _dir: dir,
        profile,
        client,
    }
}

async fn rows(client: &DuckDbClient, sql: &str) -> Vec<Vec<CellValue>> {
    match query_rows_on_client(client, sql, &[]).await {
        Ok(mut sets) => sets.remove(0).rows,
        Err(e) => panic!("DuckDB rejected generated SQL:\n{sql}\n{e}"),
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
    let s = || "sales".to_string();
    vec![
        MetadataQuery::ServerCapability,
        MetadataQuery::ServerPermissions,
        MetadataQuery::ServerConfiguration,
        MetadataQuery::ServerStatus,
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
            object: "orders".into(),
        },
        MetadataQuery::AllColumns,
        MetadataQuery::AllForeignKeys,
        MetadataQuery::AllPrimaryKeys,
        MetadataQuery::ProcedureParameters {
            schema: s(),
            object: "tax".into(),
        },
        MetadataQuery::Indexes {
            schema: s(),
            table: "orders".into(),
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
            table: "orders".into(),
        },
        MetadataQuery::Types { schema: None },
        MetadataQuery::Types { schema: Some(s()) },
        MetadataQuery::DatabaseTriggers { schema: None },
        MetadataQuery::Sequences { schema: None },
        MetadataQuery::Sequences { schema: Some(s()) },
        MetadataQuery::Synonyms { schema: None },
        MetadataQuery::Extensions,
        MetadataQuery::AvailableExtensions,
        MetadataQuery::Events { schema: None },
        MetadataQuery::MaterializedViews { schema: None },
        MetadataQuery::TriggerDefinition {
            schema: s(),
            name: "nope".into(),
        },
        MetadataQuery::ConstraintDefinition {
            schema: s(),
            table: "customers".into(),
            name: "customers_name_unique".into(),
        },
        MetadataQuery::TypeDetail {
            schema: s(),
            type_name: "mood".into(),
        },
        MetadataQuery::TableGeneral {
            schema: s(),
            table: "orders".into(),
        },
        MetadataQuery::DatabaseGeneral {
            database: "dialects".into(),
        },
        MetadataQuery::DatabaseFiles {
            database: "dialects".into(),
        },
        MetadataQuery::DatabasePermissions {
            database: "dialects".into(),
        },
        MetadataQuery::ColumnGeneral {
            schema: s(),
            table: "orders".into(),
            column: "amount".into(),
        },
        MetadataQuery::ObjectGeneral {
            schema: s(),
            object: "big_orders".into(),
        },
        MetadataQuery::IndexGeneral {
            schema: s(),
            table: "orders".into(),
            index: "orders_customer_idx".into(),
        },
        MetadataQuery::IndexColumns {
            schema: s(),
            table: "orders".into(),
            index: "orders_customer_idx".into(),
        },
        MetadataQuery::ColumnDefaults {
            schema: s(),
            table: "orders".into(),
        },
        MetadataQuery::TableIndexColumns {
            schema: s(),
            table: "orders".into(),
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
            object: "tax".into(),
        },
        MetadataQuery::ObjectPermissions {
            schema: s(),
            object: "orders".into(),
        },
    ]
}

#[tokio::test]
async fn every_metadata_query_runs_on_duckdb() {
    let fx = fixture().await;
    for query in every_query() {
        let sql = sql_for_engine(&query, DatabaseType::DuckDb).unwrap();
        let _ = rows(&fx.client, &sql).await;
    }
}

async fn run(fx: &Fixture, query: MetadataQuery) -> Vec<Vec<CellValue>> {
    let sql = sql_for_engine(&query, DatabaseType::DuckDb).unwrap();
    rows(&fx.client, &sql).await
}

#[tokio::test]
async fn catalog_queries_describe_the_fixture() {
    let fx = fixture().await;
    let dbs = run(&fx, MetadataQuery::Databases).await;
    assert_eq!(names(&dbs, 0), vec!["dialects"]);

    let schemas = run(&fx, MetadataQuery::Schemas).await;
    assert_eq!(names(&schemas, 0), vec!["main", "sales"]);

    let counts = run(&fx, MetadataQuery::SchemaObjectCounts).await;
    let sales = counts.iter().find(|r| text(&r[0]) == "sales").unwrap();
    // 2 tables + 1 view + 2 macros.
    assert_eq!(text(&sales[1]), "5");

    let tables = run(
        &fx,
        MetadataQuery::Tables {
            schema: Some("sales".into()),
        },
    )
    .await;
    assert_eq!(names(&tables, 1), vec!["customers", "orders"]);
    let all_tables = run(&fx, MetadataQuery::Tables { schema: None }).await;
    assert_eq!(all_tables.len(), 2);

    let views = run(&fx, MetadataQuery::Views { schema: None }).await;
    assert_eq!(names(&views, 1), vec!["big_orders"]);

    let routines = run(&fx, MetadataQuery::ProgrammableObjects { schema: None }).await;
    let kinds: Vec<(String, String)> = routines
        .iter()
        .map(|r| (text(&r[1]), text(&r[2])))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("recent".to_string(), "TF".to_string()),
            ("tax".to_string(), "FN".to_string())
        ]
    );

    let inventory = run(&fx, MetadataQuery::ObjectInventory { schema: None }).await;
    let kinds: Vec<String> = inventory.iter().map(|r| text(&r[0])).collect();
    assert_eq!(
        kinds,
        vec!["FUNCTION", "FUNCTION", "TABLE", "TABLE", "VIEW"]
    );
    let orders = inventory.iter().find(|r| text(&r[2]) == "orders").unwrap();
    assert_eq!(text(&orders[3]), "3", "row estimate");

    let columns = run(
        &fx,
        MetadataQuery::Columns {
            schema: "sales".into(),
            object: "orders".into(),
        },
    )
    .await;
    assert_eq!(
        names(&columns, 0),
        vec!["id", "customer_id", "amount", "placed_at"]
    );
    let amount = &columns[2];
    assert_eq!(text(&amount[1]), "DECIMAL(12,2)");
    assert_eq!(text(&amount[3]), "12");
    assert_eq!(text(&amount[4]), "2");
    assert_eq!(text(&amount[5]), "1", "nullable as 0/1");
    assert_eq!(
        text(&columns[0][6]),
        "1",
        "nextval default reads as identity"
    );

    let all_columns = run(&fx, MetadataQuery::AllColumns).await;
    assert!(all_columns.len() >= 8);
    assert!(all_columns.iter().all(|r| !text(&r[0]).is_empty()));

    let fks = run(&fx, MetadataQuery::AllForeignKeys).await;
    assert_eq!(fks.len(), 1);
    assert_eq!(
        (
            text(&fks[0][2]),
            text(&fks[0][3]),
            text(&fks[0][5]),
            text(&fks[0][6])
        ),
        (
            "orders".into(),
            "customer_id".into(),
            "customers".into(),
            "id".into()
        )
    );

    let pks = run(&fx, MetadataQuery::AllPrimaryKeys).await;
    let pk_cols: Vec<(String, String, String)> = pks
        .iter()
        .map(|r| (text(&r[1]), text(&r[2]), text(&r[3])))
        .collect();
    assert_eq!(
        pk_cols,
        vec![
            ("customers".into(), "id".into(), "1".into()),
            ("orders".into(), "id".into(), "1".into())
        ]
    );

    let params = run(
        &fx,
        MetadataQuery::ProcedureParameters {
            schema: "sales".into(),
            object: "tax".into(),
        },
    )
    .await;
    assert_eq!(names(&params, 0), vec!["amount", "rate"]);

    let indexes = run(
        &fx,
        MetadataQuery::Indexes {
            schema: "sales".into(),
            table: "orders".into(),
        },
    )
    .await;
    assert_eq!(names(&indexes, 0), vec!["orders_customer_idx"]);
    let index_columns = run(
        &fx,
        MetadataQuery::IndexColumns {
            schema: "sales".into(),
            table: "orders".into(),
            index: "orders_customer_idx".into(),
        },
    )
    .await;
    assert_eq!(names(&index_columns, 0), vec!["customer_id", "placed_at"]);
    assert_eq!(names(&index_columns, 1), vec!["1", "2"]);

    let constraints = run(
        &fx,
        MetadataQuery::Constraints {
            schema: "sales".into(),
            table: "orders".into(),
        },
    )
    .await;
    let kinds: Vec<String> = constraints.iter().map(|r| text(&r[1])).collect();
    assert!(
        kinds.contains(&"PRIMARY_KEY_CONSTRAINT".to_string()),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&"FOREIGN_KEY_CONSTRAINT".to_string()),
        "{kinds:?}"
    );
    assert!(kinds.contains(&"CHECK_CONSTRAINT".to_string()), "{kinds:?}");
    // DuckDB names constraints itself (`<table>_<column>_key`), even when the
    // DDL gave one; the explorer addresses them by the name DuckDB reports.
    let unique = run(
        &fx,
        MetadataQuery::Constraints {
            schema: "sales".into(),
            table: "customers".into(),
        },
    )
    .await;
    let unique_name = unique
        .iter()
        .find(|r| text(&r[1]) == "UNIQUE_CONSTRAINT")
        .map(|r| text(&r[0]))
        .unwrap_or_else(|| panic!("no unique constraint: {unique:?}"));
    assert!(!unique_name.is_empty());
    let definition = run(
        &fx,
        MetadataQuery::ConstraintDefinition {
            schema: "sales".into(),
            table: "customers".into(),
            name: unique_name,
        },
    )
    .await;
    assert!(
        text(&definition[0][0]).to_uppercase().contains("UNIQUE"),
        "{definition:?}"
    );

    let types = run(&fx, MetadataQuery::Types { schema: None }).await;
    assert_eq!(names(&types, 1), vec!["mood"]);
    assert_eq!(text(&types[0][2]), "ENUM");

    let sequences = run(&fx, MetadataQuery::Sequences { schema: None }).await;
    assert_eq!(names(&sequences, 1), vec!["order_seq"]);
    assert_eq!(text(&sequences[0][3]), "100");
    assert_eq!(text(&sequences[0][4]), "5");

    let defaults = run(
        &fx,
        MetadataQuery::ColumnDefaults {
            schema: "sales".into(),
            table: "orders".into(),
        },
    )
    .await;
    assert_eq!(names(&defaults, 0), vec!["id", "placed_at"]);

    let dependents = run(
        &fx,
        MetadataQuery::DependentViews {
            schema: "sales".into(),
            table: "orders".into(),
        },
    )
    .await;
    assert_eq!(names(&dependents, 1), vec!["big_orders"]);

    let view_def = run(
        &fx,
        MetadataQuery::ObjectDefinition {
            schema: "sales".into(),
            object: "big_orders".into(),
        },
    )
    .await;
    assert!(
        text(&view_def[0][0])
            .to_uppercase()
            .starts_with("CREATE VIEW")
    );
    let macro_def = run(
        &fx,
        MetadataQuery::ObjectDefinition {
            schema: "sales".into(),
            object: "tax".into(),
        },
    )
    .await;
    assert_eq!(
        text(&macro_def[0][0]),
        "CREATE MACRO tax(amount, rate) AS (amount * rate)"
    );
    let table_macro_def = run(
        &fx,
        MetadataQuery::ObjectDefinition {
            schema: "sales".into(),
            object: "recent".into(),
        },
    )
    .await;
    assert!(text(&table_macro_def[0][0]).starts_with("CREATE MACRO recent(n) AS TABLE "));

    let general = run(
        &fx,
        MetadataQuery::TableGeneral {
            schema: "sales".into(),
            table: "orders".into(),
        },
    )
    .await;
    assert_eq!(text(&general[0][4]), "3");

    let column_general = run(
        &fx,
        MetadataQuery::ColumnGeneral {
            schema: "sales".into(),
            table: "orders".into(),
            column: "amount".into(),
        },
    )
    .await;
    assert_eq!(text(&column_general[0][3]), "3", "ordinal");

    let object_general = run(
        &fx,
        MetadataQuery::ObjectGeneral {
            schema: "sales".into(),
            object: "big_orders".into(),
        },
    )
    .await;
    assert_eq!(text(&object_general[0][2]), "VIEW");

    let capability = run(&fx, MetadataQuery::ServerCapability).await;
    assert!(text(&capability[0][0]).starts_with('v'));
    assert_eq!(text(&capability[0][1]), "DuckDB");

    let files = run(
        &fx,
        MetadataQuery::DatabaseFiles {
            database: "dialects".into(),
        },
    )
    .await;
    assert_eq!(files.len(), 1);
    assert!(text(&files[0][2]).ends_with("dialects.duckdb"));

    let settings = run(&fx, MetadataQuery::ServerConfiguration).await;
    assert!(names(&settings, 0).contains(&"threads".to_string()));

    let extensions = run(&fx, MetadataQuery::Extensions).await;
    let loaded = names(&extensions, 0);
    assert!(loaded.contains(&"parquet".to_string()), "{loaded:?}");
    assert!(loaded.contains(&"json".to_string()), "{loaded:?}");
}

#[tokio::test]
async fn admin_queries_run_on_duckdb() {
    let fx = fixture().await;
    let disk = rows(
        &fx.client,
        DiskUsage::databases_query_for(DiskDialect::DuckDb),
    )
    .await;
    assert!(
        names(&disk, 0).contains(&"dialects".to_string()),
        "{disk:?}"
    );
    assert!(DiskUsage::files_query_for(DiskDialect::DuckDb).is_none());
    for sql in Dashboard::all_queries_for(DashboardDialect::DuckDb) {
        let _ = rows(&fx.client, sql).await;
    }
    let metrics = rows(
        &fx.client,
        Dashboard::metric_groups_for(DashboardDialect::DuckDb)[0].sql,
    )
    .await;
    let objects = metrics
        .iter()
        .find(|r| text(&r[0]) == "objects_total")
        .unwrap();
    assert_eq!(text(&objects[1]), "3");
    let indexes = metrics
        .iter()
        .find(|r| text(&r[0]) == "indexes_total")
        .unwrap();
    assert_eq!(text(&indexes[1]), "1");

    let probe = sqlly_services::server_info::probe_sql(DatabaseType::DuckDb).unwrap();
    let info = rows(&fx.client, probe).await;
    assert_eq!(text(&info[0][0]), "DuckDB");
    assert_eq!(text(&info[0][6]), "dialects");
    let _ = fx.profile;
}

#[tokio::test]
async fn plans_capture_and_parse_on_duckdb() {
    let fx = fixture().await;
    let sql = "SELECT c.name, sum(o.amount) FROM sales.orders o JOIN sales.customers c ON c.id = o.customer_id GROUP BY c.name";
    for (capture, actual) in [
        (estimated_capture_sql(PlanDialect::DuckDb, sql), false),
        (explain_capture_sql(PlanDialect::DuckDb, sql), true),
    ] {
        let result = rows(&fx.client, &capture).await;
        // `physical_plan` for EXPLAIN, `analyzed_plan` for EXPLAIN ANALYZE.
        assert!(text(&result[0][0]).ends_with("_plan"), "{result:?}");
        let doc = text(&result[0][1]);
        let plans = parse_captured_plan(PlanDialect::DuckDb, &doc).unwrap();
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert!(plan.nodes.len() >= 3, "{plan:?}");
        assert!(
            plan.nodes.iter().any(|n| n.logical_op.contains("SCAN")),
            "{plan:?}"
        );
        assert!(
            plan.nodes.iter().any(|n| n.logical_op.contains("JOIN")),
            "{plan:?}"
        );
        assert_eq!(
            plan.nodes.iter().any(|n| n.actual_rows.is_some()),
            actual,
            "actual rows present only for EXPLAIN ANALYZE"
        );
    }
}
