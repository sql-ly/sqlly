//! Every Oracle-specific SQL generator in the service layer, run against a
//! live Oracle Database: the catalog SQL behind the explorer and
//! IntelliSense, the disk-usage, dashboard and activity queries, the
//! server-info probe, plan capture + parsing, and the maintenance actions.
//! A generator whose SQL Oracle rejects fails here instead of in the app.
//!
//! Opt-in through `SQLLY_ORACLE_SERVER` / `SQLLY_ORACLE_USER` /
//! `SQLLY_ORACLE_PASSWORD` (optional `SQLLY_ORACLE_PORT`, `SQLLY_ORACLE_SERVICE`,
//! default `FREEPDB1`), exactly like the driver's own live tests. The
//! `V$` dashboard, activity, insight and actual-plan groups need
//! SELECT_CATALOG_ROLE on the login; without it they may fail with
//! ORA-00942/ORA-01031, which the tests accept unless
//! `SQLLY_ORACLE_EXPECT_CATALOG=1`. The second half covers the tools built
//! on the driver: the row-editing lane, Query Insights, dependency
//! discovery and actual plans.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseType, EventBus, EventPayload, ResultColumn,
    SecretProvider, SecretRef,
};
use sqlly_engines::oracle::{
    OracleClient, execute_batch_on_client, execute_on_client, open_client, query_rows_on_client,
};
use sqlly_intel::metadata::MetadataQuery;
use sqlly_intel::metadata::dialect::sql_for_engine;
use sqlly_services::admin::activity::ActivityMonitor;
use sqlly_services::admin::advanced_tools::{QueryStore, QueryStoreRank};
use sqlly_services::admin::disk_usage::{DiskDialect, DiskUsage};
use sqlly_services::admin::oracle_dependencies;
use sqlly_services::maintenance::{MaintenanceOp, MaintenanceTarget, maintenance_sql};
use sqlly_services::plans::{
    PlanDialect, estimated_capture_sql, explain_capture_sql, parse_captured_plan,
};
use sqlly_services::row_editing::{
    BatchRequest, ColumnExpression, DeleteRowOutcome, DeleteRowRequest, InsertRowOutcome,
    InsertRowRequest, RowEditOpenOutcome, RowEditOpenRequest, RowEditingService, StagedBatch,
    StagedOp, TableTarget, UpdateRowOutcome, UpdateRowRequest,
};
use sqlly_services::server_info::{parse_probe_row, probe_sql};
use sqlly_services::{ActivityDialect, Dashboard, DashboardDialect};
use sqlly_services::{BatchOutcome, DefaultRowEditingService};
use tokio::sync::OnceCell;

fn live_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "oracle-dialects",
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

/// The fixture schema, created once per test binary in the login's own
/// schema. Object names carry a `SQLLY_` prefix so a shared database is not
/// disturbed.
const SCHEMA_SQL: &[&str] = &[
    "CREATE SEQUENCE sqlly_order_seq START WITH 100 INCREMENT BY 5",
    "CREATE TABLE sqlly_customers (id NUMBER(10) PRIMARY KEY, name VARCHAR2(50) NOT NULL, email VARCHAR2(100), CONSTRAINT sqlly_customers_name_uq UNIQUE (name))",
    "COMMENT ON TABLE sqlly_customers IS 'People'",
    "CREATE TABLE sqlly_orders (id NUMBER(10) DEFAULT sqlly_order_seq.NEXTVAL PRIMARY KEY, customer_id NUMBER(10) NOT NULL, amount NUMBER(12,2), placed_at TIMESTAMP DEFAULT SYSTIMESTAMP, note CLOB, CONSTRAINT sqlly_orders_amount_ck CHECK (amount >= 0), CONSTRAINT sqlly_orders_customer_fk FOREIGN KEY (customer_id) REFERENCES sqlly_customers (id))",
    "CREATE INDEX sqlly_orders_customer_ix ON sqlly_orders (customer_id, placed_at DESC)",
    "CREATE VIEW sqlly_big_orders AS SELECT o.id, c.name, o.amount FROM sqlly_orders o JOIN sqlly_customers c ON c.id = o.customer_id WHERE o.amount > 100",
    "CREATE OR REPLACE PROCEDURE sqlly_add_customer(p_id IN NUMBER, p_name IN VARCHAR2, p_count OUT NUMBER) AS BEGIN INSERT INTO sqlly_customers (id, name) VALUES (p_id, p_name); SELECT COUNT(*) INTO p_count FROM sqlly_customers; END;",
    "CREATE OR REPLACE FUNCTION sqlly_tax(p_amount IN NUMBER, p_rate IN NUMBER DEFAULT 0.2) RETURN NUMBER AS BEGIN RETURN p_amount * p_rate; END;",
    "CREATE OR REPLACE PACKAGE sqlly_pkg AS PROCEDURE ping(p_x IN NUMBER); FUNCTION twice(p_x IN NUMBER) RETURN NUMBER; END sqlly_pkg;",
    "CREATE OR REPLACE PACKAGE BODY sqlly_pkg AS PROCEDURE ping(p_x IN NUMBER) IS BEGIN NULL; END; FUNCTION twice(p_x IN NUMBER) RETURN NUMBER IS BEGIN RETURN p_x * 2; END; END sqlly_pkg;",
    "CREATE OR REPLACE TRIGGER sqlly_orders_biu BEFORE INSERT ON sqlly_orders FOR EACH ROW BEGIN IF :NEW.amount IS NULL THEN :NEW.amount := 0; END IF; END;",
    "CREATE SYNONYM sqlly_cust FOR sqlly_customers",
    "CREATE TYPE sqlly_money AS OBJECT (amount NUMBER, currency VARCHAR2(3))",
    "INSERT INTO sqlly_customers (id, name) VALUES (1, 'Ada')",
    "INSERT INTO sqlly_customers (id, name) VALUES (2, 'Grace')",
    "INSERT INTO sqlly_orders (customer_id, amount) VALUES (1, 150)",
    "INSERT INTO sqlly_orders (customer_id, amount) VALUES (2, 20.5)",
    "INSERT INTO sqlly_orders (customer_id, amount) VALUES (1, 999.99)",
    "CREATE MATERIALIZED VIEW sqlly_order_totals AS SELECT customer_id, SUM(amount) AS total FROM sqlly_orders GROUP BY customer_id",
];

const DROP_SQL: &[&str] = &[
    "MATERIALIZED VIEW sqlly_order_totals",
    "TYPE sqlly_money",
    "SYNONYM sqlly_cust",
    "PACKAGE sqlly_pkg",
    "FUNCTION sqlly_tax",
    "PROCEDURE sqlly_add_customer",
    "VIEW sqlly_big_orders",
    "TABLE sqlly_orders",
    "TABLE sqlly_customers",
    "SEQUENCE sqlly_order_seq",
];

struct Fixture {
    profile: ConnectionProfile,
    schema: String,
}

static FIXTURE: OnceCell<Option<Fixture>> = OnceCell::const_new();

async fn fixture() -> Option<&'static Fixture> {
    FIXTURE
        .get_or_init(|| async {
            let profile = live_profile()?;
            let client = open_client(&profile).await.expect("connect to Oracle");
            for object in DROP_SQL {
                execute_batch_on_client(
                    &client,
                    &format!(
                        "BEGIN EXECUTE IMMEDIATE 'DROP {object}'; EXCEPTION WHEN OTHERS THEN NULL; END;"
                    ),
                )
                .await
                .unwrap();
            }
            for sql in SCHEMA_SQL {
                execute_batch_on_client(&client, sql)
                    .await
                    .unwrap_or_else(|e| panic!("fixture statement failed:\n{sql}\n{e}"));
            }
            let schema = match &rows(&client, "SELECT USER FROM DUAL").await[0][0] {
                CellValue::String(s) => s.clone(),
                other => panic!("USER: {other:?}"),
            };
            Some(Fixture { profile, schema })
        })
        .await
        .as_ref()
}

macro_rules! live {
    () => {
        match fixture().await {
            Some(f) => f,
            None => {
                eprintln!("skipping: SQLLY_ORACLE_SERVER/USER not set");
                return;
            }
        }
    };
}

async fn rows(client: &OracleClient, sql: &str) -> Vec<Vec<CellValue>> {
    match query_rows_on_client(client, sql, &[]).await {
        Ok(mut sets) => sets.remove(0).rows,
        Err(e) => panic!("Oracle rejected generated SQL:\n{sql}\n{e}"),
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

fn catalog_privilege_error(error: &str) -> bool {
    std::env::var("SQLLY_ORACLE_EXPECT_CATALOG").is_err()
        && (error.contains("ORA-00942") || error.contains("ORA-01031"))
}

/// Every variant of [`MetadataQuery`] with arguments that name the fixture.
fn every_query(schema: &str) -> Vec<MetadataQuery> {
    let s = || schema.to_string();
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
            object: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::AllColumns,
        MetadataQuery::AllForeignKeys,
        MetadataQuery::AllPrimaryKeys,
        MetadataQuery::ProcedureParameters {
            schema: s(),
            object: "SQLLY_ADD_CUSTOMER".into(),
        },
        MetadataQuery::Indexes {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::Triggers {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::Statistics {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::Constraints {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::Types { schema: None },
        MetadataQuery::Types { schema: Some(s()) },
        MetadataQuery::DatabaseTriggers { schema: None },
        MetadataQuery::DatabaseTriggers { schema: Some(s()) },
        MetadataQuery::Sequences { schema: None },
        MetadataQuery::Sequences { schema: Some(s()) },
        MetadataQuery::Synonyms { schema: None },
        MetadataQuery::Synonyms { schema: Some(s()) },
        MetadataQuery::Extensions,
        MetadataQuery::AvailableExtensions,
        MetadataQuery::Events { schema: None },
        MetadataQuery::MaterializedViews { schema: None },
        MetadataQuery::MaterializedViews { schema: Some(s()) },
        MetadataQuery::TriggerDefinition {
            schema: s(),
            name: "SQLLY_ORDERS_BIU".into(),
        },
        MetadataQuery::ConstraintDefinition {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
            name: "SQLLY_ORDERS_CUSTOMER_FK".into(),
        },
        MetadataQuery::TypeDetail {
            schema: s(),
            type_name: "SQLLY_MONEY".into(),
        },
        MetadataQuery::TableGeneral {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::DatabaseGeneral {
            database: "FREEPDB1".into(),
        },
        MetadataQuery::DatabaseFiles {
            database: "FREEPDB1".into(),
        },
        MetadataQuery::DatabasePermissions {
            database: "FREEPDB1".into(),
        },
        MetadataQuery::ColumnGeneral {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
            column: "AMOUNT".into(),
        },
        MetadataQuery::ObjectGeneral {
            schema: s(),
            object: "SQLLY_BIG_ORDERS".into(),
        },
        MetadataQuery::IndexGeneral {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
            index: "SQLLY_ORDERS_CUSTOMER_IX".into(),
        },
        MetadataQuery::IndexColumns {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
            index: "SQLLY_ORDERS_CUSTOMER_IX".into(),
        },
        MetadataQuery::ColumnDefaults {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::TableIndexColumns {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::DependentViews {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
        },
        MetadataQuery::StatisticGeneral {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
            statistic: "AMOUNT".into(),
        },
        MetadataQuery::StatisticColumns {
            schema: s(),
            table: "SQLLY_ORDERS".into(),
            statistic: "AMOUNT".into(),
        },
        MetadataQuery::ObjectDefinition {
            schema: s(),
            object: "SQLLY_BIG_ORDERS".into(),
        },
        MetadataQuery::ObjectDefinition {
            schema: s(),
            object: "SQLLY_PKG".into(),
        },
        MetadataQuery::ObjectDefinition {
            schema: s(),
            object: "SQLLY_CUSTOMERS".into(),
        },
        MetadataQuery::ObjectPermissions {
            schema: s(),
            object: "SQLLY_CUSTOMERS".into(),
        },
    ]
}

async fn metadata(client: &OracleClient, query: MetadataQuery) -> Vec<Vec<CellValue>> {
    let sql = sql_for_engine(&query, DatabaseType::Oracle).unwrap();
    rows(client, &sql).await
}

#[tokio::test]
async fn every_metadata_query_runs_and_describes_the_fixture() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let s = f.schema.clone();
    // Column statistics exist once the table has been analyzed.
    execute_batch_on_client(
        &client,
        &maintenance_sql(
            DatabaseType::Oracle,
            MaintenanceOp::UpdateStatistics,
            &MaintenanceTarget::Table {
                schema: Some(s.clone()),
                name: "SQLLY_ORDERS".into(),
            },
            &[],
        )
        .unwrap(),
    )
    .await
    .unwrap();

    for query in every_query(&s) {
        let _ = metadata(&client, query).await;
    }

    let schemas = metadata(&client, MetadataQuery::Schemas).await;
    assert!(names(&schemas, 0).contains(&s), "{schemas:?}");

    let tables = metadata(
        &client,
        MetadataQuery::Tables {
            schema: Some(s.clone()),
        },
    )
    .await;
    let table_names = names(&tables, 1);
    assert!(
        table_names.contains(&"SQLLY_CUSTOMERS".to_string()),
        "{table_names:?}"
    );
    assert!(table_names.contains(&"SQLLY_ORDERS".to_string()));
    assert!(
        !table_names.contains(&"SQLLY_ORDER_TOTALS".to_string()),
        "materialized view containers are not tables: {table_names:?}"
    );
    assert!(!text(&tables[0][2]).is_empty(), "create_date: {tables:?}");

    let views = metadata(
        &client,
        MetadataQuery::Views {
            schema: Some(s.clone()),
        },
    )
    .await;
    assert!(names(&views, 1).contains(&"SQLLY_BIG_ORDERS".to_string()));

    let progs = metadata(
        &client,
        MetadataQuery::ProgrammableObjects {
            schema: Some(s.clone()),
        },
    )
    .await;
    let kinds: Vec<(String, String)> = progs
        .iter()
        .map(|r| (text(&r[1]), text(&r[2])))
        .filter(|(n, _)| n.starts_with("SQLLY_"))
        .collect();
    assert!(
        kinds.contains(&("SQLLY_ADD_CUSTOMER".into(), "P".into())),
        "{kinds:?}"
    );
    assert!(kinds.contains(&("SQLLY_TAX".into(), "FN".into())));
    assert!(kinds.contains(&("SQLLY_PKG".into(), "P".into())));
    assert!(kinds.contains(&("SQLLY_ORDERS_BIU".into(), "TR".into())));

    let inventory = metadata(
        &client,
        MetadataQuery::ObjectInventory {
            schema: Some(s.clone()),
        },
    )
    .await;
    let orders = inventory
        .iter()
        .find(|r| text(&r[2]) == "SQLLY_ORDERS")
        .expect("orders in inventory");
    assert_eq!(text(&orders[0]), "TABLE");
    assert_eq!(orders[3], CellValue::I64(3), "row estimate from statistics");
    let customers = inventory
        .iter()
        .find(|r| text(&r[2]) == "SQLLY_CUSTOMERS")
        .unwrap();
    assert_eq!(text(&customers[7]), "People");

    let columns = metadata(
        &client,
        MetadataQuery::Columns {
            schema: s.clone(),
            object: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    assert_eq!(
        names(&columns, 0),
        vec!["ID", "CUSTOMER_ID", "AMOUNT", "PLACED_AT", "NOTE"]
    );
    assert_eq!(text(&columns[2][1]), "NUMBER");
    assert_eq!(columns[2][3], CellValue::I64(12), "precision");
    assert_eq!(columns[2][4], CellValue::I64(2), "scale");
    assert_eq!(columns[2][5], CellValue::I64(1), "nullable");
    assert_eq!(columns[1][5], CellValue::I64(0), "not null");
    assert_eq!(text(&columns[4][1]), "CLOB");

    let all_columns = metadata(&client, MetadataQuery::AllColumns).await;
    assert!(
        all_columns.iter().any(|r| text(&r[0]) == s
            && text(&r[1]) == "SQLLY_BIG_ORDERS"
            && text(&r[2]) == "NAME"),
        "views are included"
    );

    let fks = metadata(&client, MetadataQuery::AllForeignKeys).await;
    let fk = fks
        .iter()
        .find(|r| text(&r[0]) == "SQLLY_ORDERS_CUSTOMER_FK")
        .expect("fk");
    assert_eq!(text(&fk[2]), "SQLLY_ORDERS");
    assert_eq!(text(&fk[3]), "CUSTOMER_ID");
    assert_eq!(text(&fk[5]), "SQLLY_CUSTOMERS");
    assert_eq!(text(&fk[6]), "ID");

    let pks = metadata(&client, MetadataQuery::AllPrimaryKeys).await;
    assert!(pks.iter().any(|r| text(&r[1]) == "SQLLY_ORDERS"
        && text(&r[2]) == "ID"
        && r[3] == CellValue::I64(1)));

    let params = metadata(
        &client,
        MetadataQuery::ProcedureParameters {
            schema: s.clone(),
            object: "SQLLY_ADD_CUSTOMER".into(),
        },
    )
    .await;
    assert_eq!(names(&params, 0), vec!["P_ID", "P_NAME", "P_COUNT"]);
    assert_eq!(params[2][5], CellValue::I64(1), "OUT parameter");
    let params = metadata(
        &client,
        MetadataQuery::ProcedureParameters {
            schema: s.clone(),
            object: "SQLLY_TAX".into(),
        },
    )
    .await;
    assert_eq!(names(&params, 0), vec!["P_AMOUNT", "P_RATE"]);
    assert_eq!(params[1][6], CellValue::I64(1), "defaulted parameter");
    let params = metadata(
        &client,
        MetadataQuery::ProcedureParameters {
            schema: s.clone(),
            object: "SQLLY_PKG".into(),
        },
    )
    .await;
    assert_eq!(
        names(&params, 0),
        vec!["P_X", "P_X"],
        "both package subprograms"
    );

    let indexes = metadata(
        &client,
        MetadataQuery::Indexes {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    let ix = indexes
        .iter()
        .find(|r| text(&r[0]) == "SQLLY_ORDERS_CUSTOMER_IX")
        .unwrap();
    assert_eq!(ix[2], CellValue::I64(0));
    assert_eq!(ix[3], CellValue::I64(0));
    assert!(
        indexes.iter().any(|r| r[3] == CellValue::I64(1)),
        "primary key index"
    );

    let index_columns = metadata(
        &client,
        MetadataQuery::IndexColumns {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
            index: "SQLLY_ORDERS_CUSTOMER_IX".into(),
        },
    )
    .await;
    assert_eq!(text(&index_columns[0][0]), "CUSTOMER_ID");
    assert_eq!(index_columns[0][1], CellValue::I64(1));
    assert_eq!(index_columns[1][2], CellValue::I64(1), "DESC column");
    let table_index_columns = metadata(
        &client,
        MetadataQuery::TableIndexColumns {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    assert!(table_index_columns.len() >= 3);

    let triggers = metadata(
        &client,
        MetadataQuery::Triggers {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    assert_eq!(names(&triggers, 0), vec!["SQLLY_ORDERS_BIU"]);
    assert_eq!(triggers[0][3], CellValue::I64(0));

    let constraints = metadata(
        &client,
        MetadataQuery::Constraints {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    let kinds: Vec<(String, String)> = constraints
        .iter()
        .map(|r| (text(&r[0]), text(&r[1])))
        .collect();
    assert!(
        kinds.contains(&("SQLLY_ORDERS_AMOUNT_CK".into(), "CHECK_CONSTRAINT".into())),
        "{kinds:?}"
    );
    assert!(kinds.contains(&(
        "SQLLY_ORDERS_CUSTOMER_FK".into(),
        "FOREIGN_KEY_CONSTRAINT".into()
    )));
    assert!(kinds.iter().any(|(_, k)| k == "PRIMARY_KEY_CONSTRAINT"));
    assert!(
        !kinds
            .iter()
            .any(|(n, k)| n.starts_with("SYS_C") && k == "CHECK_CONSTRAINT"),
        "system NOT NULL checks stay out: {kinds:?}"
    );

    let types = metadata(
        &client,
        MetadataQuery::Types {
            schema: Some(s.clone()),
        },
    )
    .await;
    assert!(
        types
            .iter()
            .any(|r| text(&r[1]) == "SQLLY_MONEY" && text(&r[2]) == "OBJECT")
    );

    let sequences = metadata(
        &client,
        MetadataQuery::Sequences {
            schema: Some(s.clone()),
        },
    )
    .await;
    let seq = sequences
        .iter()
        .find(|r| text(&r[1]) == "SQLLY_ORDER_SEQ")
        .unwrap();
    assert_eq!(seq[4], CellValue::I64(5));

    let synonyms = metadata(
        &client,
        MetadataQuery::Synonyms {
            schema: Some(s.clone()),
        },
    )
    .await;
    let syn = synonyms
        .iter()
        .find(|r| text(&r[1]) == "SQLLY_CUST")
        .unwrap();
    assert_eq!(text(&syn[2]), format!("{s}.SQLLY_CUSTOMERS"));

    let mviews = metadata(
        &client,
        MetadataQuery::MaterializedViews {
            schema: Some(s.clone()),
        },
    )
    .await;
    assert!(
        mviews
            .iter()
            .any(|r| text(&r[1]) == "SQLLY_ORDER_TOTALS" && r[2] == CellValue::I64(1))
    );

    let db_triggers = metadata(
        &client,
        MetadataQuery::DatabaseTriggers {
            schema: Some(s.clone()),
        },
    )
    .await;
    assert!(
        db_triggers
            .iter()
            .any(|r| text(&r[1]) == "SQLLY_ORDERS_BIU" && text(&r[2]) == "SQLLY_ORDERS")
    );

    for query in [
        MetadataQuery::Extensions,
        MetadataQuery::AvailableExtensions,
        MetadataQuery::Events { schema: None },
    ] {
        assert!(metadata(&client, query).await.is_empty());
    }

    let def = metadata(
        &client,
        MetadataQuery::TriggerDefinition {
            schema: s.clone(),
            name: "SQLLY_ORDERS_BIU".into(),
        },
    )
    .await;
    assert!(
        text(&def[0][0]).to_ascii_uppercase().contains("TRIGGER"),
        "{def:?}"
    );
    let def = metadata(
        &client,
        MetadataQuery::ConstraintDefinition {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
            name: "SQLLY_ORDERS_CUSTOMER_FK".into(),
        },
    )
    .await;
    assert!(
        text(&def[0][0])
            .to_ascii_uppercase()
            .contains("FOREIGN KEY"),
        "{def:?}"
    );
    for (object, needle) in [
        ("SQLLY_BIG_ORDERS", "VIEW"),
        ("SQLLY_PKG", "PACKAGE BODY"),
        ("SQLLY_CUSTOMERS", "CREATE TABLE"),
        ("SQLLY_TAX", "FUNCTION"),
    ] {
        let def = metadata(
            &client,
            MetadataQuery::ObjectDefinition {
                schema: s.clone(),
                object: object.into(),
            },
        )
        .await;
        assert_eq!(def.len(), 1, "{object}: {def:?}");
        assert!(
            text(&def[0][0]).to_ascii_uppercase().contains(needle),
            "{object}: {}",
            text(&def[0][0])
        );
    }

    let dependents = metadata(
        &client,
        MetadataQuery::DependentViews {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    let view = dependents
        .iter()
        .find(|r| text(&r[1]) == "SQLLY_BIG_ORDERS")
        .expect("dependent view");
    assert!(text(&view[3]).to_ascii_uppercase().contains("SELECT"));

    let defaults = metadata(
        &client,
        MetadataQuery::ColumnDefaults {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    let by_name: Vec<(String, String)> = defaults
        .iter()
        .map(|r| (text(&r[0]), text(&r[1]).to_ascii_uppercase()))
        .collect();
    assert!(
        by_name
            .iter()
            .any(|(n, d)| n == "ID" && d.contains("NEXTVAL")),
        "{by_name:?}"
    );
    assert!(
        by_name
            .iter()
            .any(|(n, d)| n == "PLACED_AT" && d.contains("SYSTIMESTAMP"))
    );

    let stats = metadata(
        &client,
        MetadataQuery::Statistics {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    assert!(
        names(&stats, 0).contains(&"AMOUNT".to_string()),
        "{stats:?}"
    );

    let general = metadata(
        &client,
        MetadataQuery::TableGeneral {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
        },
    )
    .await;
    assert_eq!(general[0][4], CellValue::I64(3));
    let column = metadata(
        &client,
        MetadataQuery::ColumnGeneral {
            schema: s.clone(),
            table: "SQLLY_ORDERS".into(),
            column: "AMOUNT".into(),
        },
    )
    .await;
    assert_eq!(column[0][6], CellValue::I64(12));
    assert_eq!(column[0][7], CellValue::I64(2));
    let object = metadata(
        &client,
        MetadataQuery::ObjectGeneral {
            schema: s.clone(),
            object: "SQLLY_BIG_ORDERS".into(),
        },
    )
    .await;
    assert_eq!(text(&object[0][2]), "VIEW");

    let counts = metadata(&client, MetadataQuery::SchemaObjectCounts).await;
    let mine = counts.iter().find(|r| text(&r[0]) == s).unwrap();
    assert!(matches!(mine[1], CellValue::I64(n) if n >= 6), "{mine:?}");

    let databases = metadata(&client, MetadataQuery::Databases).await;
    assert_eq!(databases.len(), 1);
    assert!(!text(&databases[0][0]).is_empty());
    let capability = metadata(&client, MetadataQuery::ServerCapability).await;
    assert!(
        text(&capability[0][0])
            .chars()
            .next()
            .unwrap()
            .is_ascii_digit()
    );
    let permissions = metadata(
        &client,
        MetadataQuery::ObjectPermissions {
            schema: s.clone(),
            object: "SQLLY_CUSTOMERS".into(),
        },
    )
    .await;
    assert!(names(&permissions, 0).contains(&"OWNER".to_string()));
    assert!(
        !metadata(&client, MetadataQuery::ServerConfiguration)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn disk_dashboard_activity_and_probe_queries_run() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();

    let disk = rows(&client, DiskUsage::databases_query_for(DiskDialect::Oracle)).await;
    assert!(!disk.is_empty(), "the fixture owns segments");
    assert!(!text(&disk[0][0]).is_empty(), "tablespace name");

    for group in Dashboard::metric_groups_for(DashboardDialect::Oracle) {
        match query_rows_on_client(&client, group.sql, &[]).await {
            Ok(sets) => {
                assert!(
                    !sets[0].rows.is_empty(),
                    "group {} returned no metrics",
                    group.name
                );
                for row in &sets[0].rows {
                    assert!(!text(&row[0]).is_empty(), "metric key in {}", group.name);
                }
            }
            Err(error) => assert!(
                catalog_privilege_error(&error.to_string()),
                "group {} failed: {error}",
                group.name
            ),
        }
    }

    for sql in [
        ActivityMonitor::sessions_query_for(ActivityDialect::Oracle),
        ActivityMonitor::blocking_query_for(ActivityDialect::Oracle),
        ActivityMonitor::expensive_queries_query_for(ActivityDialect::Oracle),
        ActivityMonitor::waits_query_for(ActivityDialect::Oracle),
    ] {
        if let Err(error) = query_rows_on_client(&client, sql, &[]).await {
            assert!(
                catalog_privilege_error(&error.to_string()),
                "{sql}\n{error}"
            );
        }
    }
    // A kill for a session that does not exist is a no-op block, and the
    // diagnostics script is two runnable SELECTs.
    let kill = ActivityMonitor::kill_session_sql_for(ActivityDialect::Oracle, 999_999);
    if let Err(error) = execute_batch_on_client(&client, &kill).await {
        assert!(
            catalog_privilege_error(&error.to_string()),
            "{kill}\n{error}"
        );
    }
    let diagnostics = ActivityMonitor::diagnostic_sql_for(ActivityDialect::Oracle, 1);
    if let Err(error) = execute_batch_on_client(&client, &diagnostics).await {
        assert!(
            catalog_privilege_error(&error.to_string()),
            "{diagnostics}\n{error}"
        );
    }

    let probe = rows(&client, probe_sql(DatabaseType::Oracle).unwrap()).await;
    assert_eq!(probe.len(), 1);
    let info = parse_probe_row(DatabaseType::Oracle, &probe[0]);
    assert_eq!(info.product, "Oracle Database");
    assert!(
        info.version.chars().next().unwrap().is_ascii_digit(),
        "{info:?}"
    );
    assert_eq!(info.current_user.as_deref(), Some(f.schema.as_str()));
    assert!(info.database.is_some());
    assert_eq!(info.tls_in_use, Some(false));
}

#[tokio::test]
async fn plan_capture_parses_into_a_tree() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let sql = estimated_capture_sql(
        PlanDialect::Oracle,
        "SELECT o.id, c.name FROM sqlly_orders o JOIN sqlly_customers c ON c.id = o.customer_id WHERE c.name = 'Ada'",
    );
    let events = EventBus::new(1024);
    let mut sub = events.subscribe();
    execute_on_client(&client, &f.profile, &sql, &events, None)
        .await
        .unwrap();
    let mut lines = Vec::new();
    while let Ok(event) = sub.try_recv() {
        if let EventPayload::Rows { rows, .. } = event.payload {
            for row in rows {
                if let Some(CellValue::String(line)) = row.first() {
                    lines.push(line.clone());
                }
            }
        }
    }
    assert!(lines.len() >= 3, "{lines:?}");
    let plans = parse_captured_plan(PlanDialect::Oracle, &lines.join("\n")).unwrap();
    let plan = &plans[0];
    assert_eq!(plan.nodes[0].logical_op, "SELECT STATEMENT");
    assert!(!plan.edges.is_empty());
    assert!(
        plan.nodes
            .iter()
            .any(|n| n.physical_op.contains("SQLLY_ORDERS")
                || n.physical_op.contains("SQLLY_CUSTOMERS")),
        "{:?}",
        plan.nodes
            .iter()
            .map(|n| &n.physical_op)
            .collect::<Vec<_>>()
    );
    assert!(plan.nodes.iter().any(|n| n.estimated_cost > 0.0));
}

#[tokio::test]
async fn maintenance_statements_run() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let schema = Some(f.schema.clone());
    let cases = [
        (
            MaintenanceOp::UpdateStatistics,
            MaintenanceTarget::Database {
                name: "FREEPDB1".into(),
            },
        ),
        (
            MaintenanceOp::UpdateStatistics,
            MaintenanceTarget::Table {
                schema: schema.clone(),
                name: "SQLLY_ORDERS".into(),
            },
        ),
        (
            MaintenanceOp::CheckIntegrity,
            MaintenanceTarget::Table {
                schema: schema.clone(),
                name: "SQLLY_ORDERS".into(),
            },
        ),
        (
            MaintenanceOp::Reindex,
            MaintenanceTarget::Table {
                schema: schema.clone(),
                name: "SQLLY_ORDERS".into(),
            },
        ),
        (
            MaintenanceOp::Reindex,
            MaintenanceTarget::Index {
                schema: schema.clone(),
                table: "SQLLY_ORDERS".into(),
                name: "SQLLY_ORDERS_CUSTOMER_IX".into(),
            },
        ),
    ];
    for (op, target) in cases {
        let sql = maintenance_sql(DatabaseType::Oracle, op, &target, &[]).unwrap();
        execute_batch_on_client(&client, &sql)
            .await
            .unwrap_or_else(|e| panic!("{op:?} on {target:?} failed:\n{sql}\n{e}"));
    }
}

// ── Tools enabled on top of the driver ────────────────────────────────────

fn col(name: &str) -> ResultColumn {
    ResultColumn {
        name: name.to_string(),
        data_type: "varchar2".to_string(),
        nullable: true,
    }
}

fn s(text: &str) -> CellValue {
    CellValue::String(text.to_string())
}

fn cell_named(columns: &[String], row: &[CellValue], name: &str) -> CellValue {
    let index = columns
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no column {name} in {columns:?}"));
    row[index].clone()
}

/// Row editing end to end on the guarded lane: provenance from the parsed
/// statement plus the server-side describe, primary-key and ROWID keys,
/// typed temporal binds, DEFAULT expressions, optimistic concurrency, the
/// ROWID-returning insert, staged batches with typeless binds, and the
/// honest refusals (joins, views).
#[tokio::test]
async fn row_editing_round_trips_on_primary_keys_and_rowid() {
    let f = live!();
    let service = DefaultRowEditingService::default();
    let client = open_client(&f.profile).await.unwrap();

    // Open by primary key; the statement names the table unqualified.
    let sql = "SELECT id, name, email FROM sqlly_customers ORDER BY id";
    let outcome = service
        .open_session(
            &f.profile,
            RowEditOpenRequest {
                sql: sql.into(),
                result_set_index: 0,
                columns: vec![col("ID"), col("NAME"), col("EMAIL")],
                row: vec![CellValue::I64(2), s("Grace"), CellValue::Null],
            },
        )
        .await
        .unwrap();
    let session = match outcome {
        RowEditOpenOutcome::Editable(session) => session,
        RowEditOpenOutcome::NotEditable { reason, .. } => panic!("not editable: {reason}"),
    };
    assert_eq!(session.target.schema, f.schema);
    assert_eq!(session.target.table, "SQLLY_CUSTOMERS");
    assert_eq!(session.key_columns, vec!["ID".to_string()]);
    let name = session.columns.iter().find(|c| c.name == "NAME").unwrap();
    assert_eq!(name.result_ordinal, Some(1));
    assert_eq!(name.char_max_length, Some(50));
    assert!(!name.nullable);
    assert_eq!(name.data_type, "varchar2");
    assert!(
        session
            .incoming_foreign_keys
            .iter()
            .any(|fk| fk.child.table == "SQLLY_ORDERS"),
        "{:?}",
        session.incoming_foreign_keys
    );
    assert_eq!(session.original_row[1], s("Grace"));

    // Guarded UPDATE with a DEFAULT expression on a sibling table later;
    // here a plain change guarded by the original value.
    let updated = service
        .update_row(
            &f.profile,
            UpdateRowRequest {
                target: session.target.clone(),
                key: vec![("ID".into(), CellValue::I64(2))],
                rowversion: None,
                changes: vec![("EMAIL".into(), s("grace@example.com"))],
                original_values: vec![("EMAIL".into(), CellValue::Null)],
                expressions: vec![],
            },
        )
        .await
        .unwrap();
    match updated {
        UpdateRowOutcome::Updated { columns, row } => {
            assert_eq!(cell_named(&columns, &row, "EMAIL"), s("grace@example.com"));
            assert!(columns.iter().any(|c| c == "ROWID"));
        }
        other => panic!("{other:?}"),
    }
    // The same change again conflicts: the original value no longer matches.
    let conflict = service
        .update_row(
            &f.profile,
            UpdateRowRequest {
                target: session.target.clone(),
                key: vec![("ID".into(), CellValue::I64(2))],
                rowversion: None,
                changes: vec![("EMAIL".into(), s("other@example.com"))],
                original_values: vec![("EMAIL".into(), CellValue::Null)],
                expressions: vec![],
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(conflict, UpdateRowOutcome::Conflict { .. }),
        "{conflict:?}"
    );

    // INSERT into the orders table: the id comes from a sequence default,
    // the timestamp is a typed bind, and the row comes back through ROWID.
    let orders = TableTarget {
        schema: f.schema.clone(),
        table: "SQLLY_ORDERS".into(),
    };
    let inserted = service
        .insert_row(
            &f.profile,
            InsertRowRequest {
                target: orders.clone(),
                values: vec![
                    ("CUSTOMER_ID".into(), CellValue::I64(2)),
                    ("AMOUNT".into(), s("12.5")),
                    ("PLACED_AT".into(), s("2024-01-02 03:04:05")),
                ],
                expressions: vec![],
            },
        )
        .await
        .unwrap();
    let InsertRowOutcome::Inserted { columns, row } = inserted;
    let new_id = match cell_named(&columns, &row, "ID") {
        CellValue::I64(id) => id,
        other => panic!("{other:?}"),
    };
    assert!(new_id >= 100, "sequence default resolved: {new_id}");
    match cell_named(&columns, &row, "PLACED_AT") {
        CellValue::String(text) => assert!(text.starts_with("2024-01-02 03:04:05"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        cell_named(&columns, &row, "AMOUNT"),
        CellValue::String("12.5".into())
    );

    // DEFAULT resets the timestamp to SYSTIMESTAMP; a typeless staged batch
    // binds a date string through the session's NLS formats.
    let defaulted = service
        .update_row(
            &f.profile,
            UpdateRowRequest {
                target: orders.clone(),
                key: vec![("ID".into(), CellValue::I64(new_id))],
                rowversion: None,
                changes: vec![("AMOUNT".into(), s("13"))],
                original_values: vec![("AMOUNT".into(), s("12.5"))],
                expressions: vec![("PLACED_AT".into(), ColumnExpression::Default)],
            },
        )
        .await
        .unwrap();
    match defaulted {
        UpdateRowOutcome::Updated { columns, row } => match cell_named(&columns, &row, "PLACED_AT")
        {
            CellValue::String(text) => assert!(!text.starts_with("2024-01-02"), "{text}"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    let batch = service
        .apply_batch(
            &f.profile,
            BatchRequest {
                batch: StagedBatch {
                    target: Some(orders.clone()),
                    key_columns: vec!["ID".into()],
                    ops: vec![StagedOp::Update(UpdateRowRequest {
                        target: orders.clone(),
                        key: vec![("ID".into(), CellValue::I64(new_id))],
                        rowversion: None,
                        changes: vec![("PLACED_AT".into(), s("2023-05-06 07:08:09"))],
                        original_values: vec![],
                        expressions: vec![],
                    })],
                },
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(batch, BatchOutcome::Committed { statements: 1 }),
        "{batch:?}"
    );
    let placed_sql = format!(
        "SELECT TO_CHAR(placed_at, 'YYYY-MM-DD HH24:MI:SS') FROM sqlly_orders WHERE id = {new_id}"
    );
    let placed = rows(&client, &placed_sql).await;
    assert_eq!(text(&placed[0][0]), "2023-05-06 07:08:09");

    // DELETE by key, guarded by the amount it was inserted with.
    let deleted = service
        .delete_row(
            &f.profile,
            DeleteRowRequest {
                target: orders.clone(),
                key: vec![("ID".into(), CellValue::I64(new_id))],
                rowversion: None,
                original_values: vec![("AMOUNT".into(), CellValue::I64(13))],
            },
        )
        .await
        .unwrap();
    assert!(matches!(deleted, DeleteRowOutcome::Deleted), "{deleted:?}");
    assert!(
        service
            .fetch_row(
                &f.profile,
                &orders,
                &[("ID".into(), CellValue::I64(new_id))]
            )
            .await
            .unwrap()
            .is_none()
    );

    // A table without a primary key is keyed by ROWID, which the statement
    // must project.
    execute_batch_on_client(
        &client,
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE sqlly_nopk'; EXCEPTION WHEN OTHERS THEN NULL; END;",
    )
    .await
    .unwrap();
    execute_batch_on_client(&client, "CREATE TABLE sqlly_nopk (v VARCHAR2(10), d DATE)")
        .await
        .unwrap();
    execute_batch_on_client(
        &client,
        "INSERT INTO sqlly_nopk (v, d) VALUES ('a', DATE '2024-03-04'); INSERT INTO sqlly_nopk (v, d) VALUES ('b', NULL)",
    )
    .await
    .unwrap();
    let rowid = text(&rows(&client, "SELECT ROWID FROM sqlly_nopk WHERE v = 'a'").await[0][0]);
    let outcome = service
        .open_session(
            &f.profile,
            RowEditOpenRequest {
                sql: "SELECT ROWID, v, d FROM sqlly_nopk".into(),
                result_set_index: 0,
                columns: vec![col("ROWID"), col("V"), col("D")],
                row: vec![s(&rowid), s("a"), s("2024-03-04 00:00:00")],
            },
        )
        .await
        .unwrap();
    let session = match outcome {
        RowEditOpenOutcome::Editable(session) => session,
        RowEditOpenOutcome::NotEditable { reason, .. } => panic!("not editable: {reason}"),
    };
    assert_eq!(session.key_columns, vec!["ROWID".to_string()]);
    assert!(session.columns[0].is_primary_key && session.columns[0].is_computed);
    assert_eq!(session.original_row[1], s("a"));
    let updated = service
        .update_row(
            &f.profile,
            UpdateRowRequest {
                target: session.target.clone(),
                key: vec![("ROWID".into(), s(&rowid))],
                rowversion: None,
                changes: vec![("V".into(), s("z")), ("D".into(), s("2025-06-07 08:09:10"))],
                original_values: vec![("V".into(), s("a")), ("D".into(), s("2024-03-04 00:00:00"))],
                expressions: vec![],
            },
        )
        .await
        .unwrap();
    match updated {
        UpdateRowOutcome::Updated { columns, row } => {
            assert_eq!(cell_named(&columns, &row, "V"), s("z"));
            assert_eq!(cell_named(&columns, &row, "D"), s("2025-06-07 08:09:10"));
        }
        other => panic!("{other:?}"),
    }
    let deleted = service
        .delete_row(
            &f.profile,
            DeleteRowRequest {
                target: session.target.clone(),
                key: vec![("ROWID".into(), s(&rowid))],
                rowversion: None,
                original_values: vec![("V".into(), s("z"))],
            },
        )
        .await
        .unwrap();
    assert!(matches!(deleted, DeleteRowOutcome::Deleted), "{deleted:?}");
    assert_eq!(
        text(&rows(&client, "SELECT COUNT(*) FROM sqlly_nopk").await[0][0]),
        "1"
    );
    // Without ROWID in the projection the row cannot be identified.
    let refused = service
        .open_session(
            &f.profile,
            RowEditOpenRequest {
                sql: "SELECT v FROM sqlly_nopk".into(),
                result_set_index: 0,
                columns: vec![col("V")],
                row: vec![s("b")],
            },
        )
        .await
        .unwrap();
    match refused {
        RowEditOpenOutcome::NotEditable { reason, .. } => {
            assert!(reason.contains("ROWID"), "{reason}")
        }
        RowEditOpenOutcome::Editable(_) => panic!("editable without a key"),
    }
    execute_batch_on_client(&client, "DROP TABLE sqlly_nopk")
        .await
        .unwrap();

    // Joins and views are refused with a reason, never an error.
    for (sql, columns, row, needle) in [
        (
            "SELECT o.id, c.name FROM sqlly_orders o JOIN sqlly_customers c ON c.id = o.customer_id",
            vec![col("ID"), col("NAME")],
            vec![CellValue::I64(100), s("Ada")],
            "more than one table",
        ),
        (
            "SELECT id, name, amount FROM sqlly_big_orders",
            vec![col("ID"), col("NAME"), col("AMOUNT")],
            vec![CellValue::I64(100), s("Ada"), s("150")],
            "not a base table",
        ),
    ] {
        let outcome = service
            .open_session(
                &f.profile,
                RowEditOpenRequest {
                    sql: sql.into(),
                    result_set_index: 0,
                    columns,
                    row,
                },
            )
            .await
            .unwrap();
        match outcome {
            RowEditOpenOutcome::NotEditable { reason, .. } => {
                assert!(reason.contains(needle), "{sql}: {reason}")
            }
            RowEditOpenOutcome::Editable(_) => panic!("{sql} should be read-only"),
        }
    }
    // Restore the fixture row.
    execute_batch_on_client(
        &client,
        "UPDATE sqlly_customers SET email = NULL WHERE id = 2",
    )
    .await
    .unwrap();
}

/// Query Insights reads the shared pool for every ranking and the cached
/// plan of one SQL_ID.
#[tokio::test]
async fn query_insights_read_the_shared_pool() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let marker = "sqlly-insights-fixture";
    rows(
        &client,
        &format!("SELECT /* {marker} */ COUNT(*) FROM sqlly_orders"),
    )
    .await;
    let mut sql_id = None;
    for rank in [
        QueryStoreRank::AvgDuration,
        QueryStoreRank::ExecutionCount,
        QueryStoreRank::AvgLogicalReads,
    ] {
        let sql = QueryStore::oracle_top_query(500, rank);
        let sets = match query_rows_on_client(&client, &sql, &[]).await {
            Ok(sets) => sets,
            Err(e) if catalog_privilege_error(&e.to_string()) => return,
            Err(e) => panic!("Oracle rejected the insights query:\n{sql}\n{e}"),
        };
        let set = &sets[0];
        let names: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
        for expected in [
            "QUERY_ID",
            "QUERY_SQL_TEXT",
            "AVG_DURATION_MS",
            "COUNT_EXECUTIONS",
            "AVG_LOGICAL_READS",
            "PARSING_SCHEMA_NAME",
        ] {
            assert!(names.contains(&expected), "{names:?}");
        }
        assert!(!set.rows.is_empty());
        let text_at = names.iter().position(|n| *n == "QUERY_SQL_TEXT").unwrap();
        if let Some(row) = set
            .rows
            .iter()
            .find(|row| text(&row[text_at]).contains(marker))
        {
            sql_id = Some(text(&row[0]));
        }
    }
    let sql_id = sql_id.expect("the fixture statement is in the shared pool");
    let plan = rows(&client, &QueryStore::oracle_plan_query(&sql_id)).await;
    let joined = names(&plan, 0).join("\n");
    assert!(joined.contains("SQLLY_ORDERS"), "{joined}");
}

/// View Dependencies / Find Usages: ALL_DEPENDENCIES both ways, foreign
/// keys, and stored-source search.
#[tokio::test]
async fn dependencies_and_usages_queries_run() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let deps = rows(
        &client,
        &oracle_dependencies::dependencies_sql(&f.schema, "SQLLY_ORDERS"),
    )
    .await;
    let triples: Vec<(String, String, String)> = deps
        .iter()
        .map(|r| (text(&r[0]), text(&r[1]), text(&r[2])))
        .collect();
    let view = format!("{}.SQLLY_BIG_ORDERS", f.schema);
    let parent = format!("{}.SQLLY_CUSTOMERS", f.schema);
    assert!(
        triples
            .iter()
            .any(|(d, o, t)| d == "Used by" && o == &view && t == "VIEW"),
        "{triples:?}"
    );
    assert!(
        triples
            .iter()
            .any(|(d, o, t)| d == "Uses" && o == &parent && t.starts_with("FOREIGN KEY ")),
        "{triples:?}"
    );
    // The unqualified form defaults to the current schema.
    let same = rows(
        &client,
        &oracle_dependencies::dependencies_sql("", "SQLLY_ORDERS"),
    )
    .await;
    assert_eq!(same.len(), deps.len());
    let usages = rows(
        &client,
        &oracle_dependencies::usages_sql(&f.schema, "SQLLY_CUSTOMERS", 500),
    )
    .await;
    let objects = names(&usages, 0);
    assert!(
        objects.contains(&format!("{}.SQLLY_ADD_CUSTOMER", f.schema)),
        "{objects:?}"
    );
    assert!(objects.contains(&view), "{objects:?}");
    assert!(
        !objects.contains(&parent),
        "the object itself is excluded: {objects:?}"
    );
}

/// Actual plans: the statement runs with row-source statistics and the
/// cursor's plan comes back with real row counts.
#[tokio::test]
async fn actual_plan_capture_reports_real_row_counts() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let sql = explain_capture_sql(
        PlanDialect::Oracle,
        "SELECT o.id, c.name FROM sqlly_orders o JOIN sqlly_customers c ON c.id = o.customer_id WHERE c.name = 'Ada'",
    );
    assert!(sql.contains("STATISTICS_LEVEL = ALL"));
    let events = EventBus::new(1024);
    let mut sub = events.subscribe();
    if let Err(e) = execute_on_client(&client, &f.profile, &sql, &events, None).await {
        if catalog_privilege_error(&e.to_string()) {
            return;
        }
        panic!("actual plan capture failed:\n{sql}\n{e}");
    }
    let mut lines = Vec::new();
    while let Ok(event) = sub.try_recv() {
        if let EventPayload::Rows { rows, .. } = event.payload {
            for row in rows {
                if let Some(CellValue::String(line)) = row.first() {
                    lines.push(line.clone());
                }
            }
        }
    }
    assert!(lines.len() >= 3, "{lines:?}");
    let plans = parse_captured_plan(PlanDialect::Oracle, &lines.join("\n")).unwrap();
    let plan = &plans[0];
    assert_eq!(plan.statement_type, "ACTUAL PLAN");
    assert_eq!(plan.nodes[0].logical_op, "SELECT STATEMENT");
    let scanned: Vec<_> = plan
        .nodes
        .iter()
        .filter(|n| {
            n.physical_op.contains("SQLLY_ORDERS") || n.physical_op.contains("SQLLY_CUSTOMERS")
        })
        .collect();
    assert!(!scanned.is_empty(), "{:?}", plan.nodes);
    assert!(
        scanned
            .iter()
            .any(|n| n.actual_rows.is_some_and(|r| r > 0.0)),
        "{:?}",
        scanned
            .iter()
            .map(|n| (&n.physical_op, n.actual_rows))
            .collect::<Vec<_>>()
    );
    assert!(
        plan.nodes
            .iter()
            .any(|n| n.output_list.iter().any(|l| l.starts_with("actual: "))),
        "{:?}",
        plan.nodes
    );
}
